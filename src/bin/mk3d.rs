//! The Maschine MK3 driver daemon.
//!
//! Thread layout, chosen around the latency budget:
//!
//! * **core** -- `SCHED_FIFO`. Polls the HID node and the ALSA sequencer input
//!   in a single `poll()`, so both directions are served by one thread that
//!   owns the engine outright and never takes a lock on the hot path. A pad
//!   hit becomes a MIDI event without a context switch.
//! * **surface** -- normal priority. Pushes LED reports and repaints the
//!   screens, rate-limited. It reads snapshots the core thread leaves behind
//!   with `try_lock`, so a busy surface thread can never stall input.
//! * **watcher** -- reloads the config when the file changes.
//! * **gui** -- serves the configuration web interface.

use anyhow::{Context, Result};
use maschine_mk3::config::Config;
use maschine_mk3::device::HidDev;
use maschine_mk3::display::{Displays, Frame};
use maschine_mk3::engine::Engine;
use maschine_mk3::hid::{self, ControlState, PadHit};
use maschine_mk3::leds::{Leds, LED_COUNT};
use maschine_mk3::midi::{MidiIo, Msg};
use maschine_mk3::preset;
use maschine_mk3::profile::Profile;
use maschine_mk3::ipc::{self, Broadcaster, Event as IpcEvent, PresetEntry, Reply, Request};
use maschine_mk3::{gui, rt, ui};
use std::path::PathBuf;
use std::os::unix::io::AsFd as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// State the core thread publishes for the surface thread to consume.
///
/// Every field is written with `try_lock` from the core thread: dropping an
/// update is always better than delaying a pad hit, and both fields are
/// idempotent snapshots where the newest value is the only one that matters.
struct Shared {
    leds: Mutex<[u8; LED_COUNT]>,
    outputs: Mutex<maschine_mk3::engine::Outputs>,
    /// Set by the watcher and the GUI; consumed by the core thread.
    pending_config: Mutex<Option<Config>>,
    /// Latest config, for the GUI to read and write.
    config: Mutex<Config>,
    /// Path the config came from.
    path: PathBuf,
    /// Hardware description in force.
    profile: Profile,
    /// Where the profile came from.
    profile_path: PathBuf,
    running: AtomicBool,
    /// Bumped whenever the core thread changes LED state.
    leds_generation: std::sync::atomic::AtomicU64,
    /// Live hardware events, for the configuration app.
    broadcast: Broadcaster,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut write_default = false;
    let mut list_ports = false;
    let mut list_presets = false;
    let mut load_preset: Option<String> = None;
    let mut save_preset: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => path = args.next().map(PathBuf::from),
            "--write-default-config" => write_default = true,
            "--list-ports" => list_ports = true,
            "--list-presets" => list_presets = true,
            "--preset" => load_preset = args.next(),
            "--save-preset" => save_preset = args.next(),
            "-h" | "--help" => {
                eprintln!(
                    "mk3d [-c CONFIG] [OPTIONS]\n\n\
                     \x20 --preset NAME           load a preset and run with it\n\
                     \x20 --save-preset NAME      save the current config as a preset, then exit\n\
                     \x20 --list-presets          show what is available\n\
                     \x20 --list-ports            show sequencer destinations\n\
                     \x20 --write-default-config  write a fresh config file\n\n\
                     Default config path: {}",
                    Config::default_path().display()
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument `{other}` (try --help)"),
        }
    }
    let path = path.unwrap_or_else(Config::default_path);

    if write_default {
        maschine_mk3::config_default::install(&path)?;
        eprintln!("wrote {}", path.display());
        return Ok(());
    }

    if list_presets {
        let active = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<Config>(&t).ok())
            .and_then(|c| c.preset)
            .map(|p| p.name);
        println!("Presets (user files shadow built-in ones of the same name):\n");
        for e in preset::list() {
            let mark = if active.as_deref() == Some(e.name.as_str()) {
                "*"
            } else {
                " "
            };
            let src = match (e.origin, e.shadows_builtin) {
                (preset::Origin::Builtin, _) => "built-in",
                (preset::Origin::User, true) => "yours, shadows built-in",
                (preset::Origin::User, false) => "yours",
            };
            println!("{mark} {:<12} {:<24} {}", e.name, format!("({src})"), e.description);
        }
        println!("\nUser presets live in {}", preset::dir().display());
        println!("Load one with:  mk3d --preset NAME");
        return Ok(());
    }

    let profile_path = Profile::default_path();
    let profile = Profile::load_or_builtin(&profile_path)?;
    eprintln!(
        "[mk3d] device: {} ({} controls, {} from {})",
        profile.device.name,
        profile.control.len(),
        if profile_path.exists() { "profile" } else { "built-in profile" },
        if profile_path.exists() {
            profile_path.display().to_string()
        } else {
            "the driver".into()
        }
    );

    if let Some(name) = &save_preset {
        let out = preset::save_from(name, "", &path)?;
        eprintln!("saved the current config as `{name}` in {}", out.display());
        eprintln!("edit its [preset] description so a chooser can say what it is");
        return Ok(());
    }

    if let Some(name) = &load_preset {
        preset::load_into(name, &path, &profile)?;
        eprintln!("[mk3d] loaded preset `{name}`");
    }

    let cfg = if path.exists() {
        Config::load(&path)?
    } else {
        eprintln!(
            "[mk3d] {} not found, writing a starter config",
            path.display()
        );
        maschine_mk3::config_default::install(&path)?;
        Config::load(&path)?
    };

    if cfg.general.lock_memory {
        rt::lock_memory();
    }

    let mut hid_in = HidDev::open()?;
    let hid_out = hid_in.try_clone()?;
    eprintln!("[mk3d] HID: {}", hid_in.path().display());

    let midi = MidiIo::open(
        &cfg.general.client_name,
        &cfg.general.out_port,
        &cfg.general.in_port,
    )?;
    let (c, p) = midi.out_addr();
    eprintln!("[mk3d] MIDI out: {c}:{p} \"{}\"", cfg.general.out_port);
    let (c, p) = midi.in_addr();
    eprintln!("[mk3d] MIDI in:  {c}:{p} \"{}\"", cfg.general.in_port);

    if list_ports {
        eprintln!("[mk3d] sequencer destinations that could receive our output:");
        for (c, p, name) in midi.destinations() {
            eprintln!("         {c:>3}:{p:<3} {name}");
        }
        eprintln!(
            "\nAdd any of these to general.connect_to in {} to subscribe them\n\
             automatically, e.g.  connect_to = [\"REAPER\"]",
            path.display()
        );
        return Ok(());
    }

    if !cfg.general.connect_to.is_empty() {
        let done = midi.connect_to_matching(&cfg.general.connect_to);
        if done.is_empty() {
            eprintln!(
                "[mk3d] general.connect_to matched nothing; run `mk3d --list-ports` \
                 to see what is available"
            );
        } else {
            for name in done {
                eprintln!("[mk3d] connected output to {name}");
            }
        }
    }

    let shared = Arc::new(Shared {
        leds: Mutex::new([0; LED_COUNT]),
        outputs: Mutex::new(Default::default()),
        pending_config: Mutex::new(None),
        config: Mutex::new(cfg.clone()),
        path: path.clone(),
        profile: profile.clone(),
        profile_path: profile_path.clone(),
        running: AtomicBool::new(true),
        leds_generation: std::sync::atomic::AtomicU64::new(0),
        broadcast: Broadcaster::new(),
    });

    install_signal_handler();

    let surface = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("mk3-surface".into())
            .spawn(move || {
                if let Err(e) = surface_thread(shared, hid_out) {
                    eprintln!("[surface] stopped: {e:#}");
                }
            })?
    };

    let watcher = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("mk3-watch".into())
            .spawn(move || {
                if let Err(e) = watch_thread(shared) {
                    eprintln!("[watch] stopped: {e:#}");
                }
            })?
    };

    let ipc_thread = {
        let shared = shared.clone();
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        // Mirror the shutdown flag, so stopping the driver closes the socket.
        let mirror = shared.clone();
        std::thread::Builder::new()
            .name("mk3-ipc-flag".into())
            .spawn(move || {
                while mirror.running.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(150));
                }
                flag.store(false, Ordering::SeqCst);
            })?;
        let broadcaster = shared.broadcast.clone();
        let path = ipc::socket_path();
        let get = {
            let s = shared.clone();
            move || {
                // The file itself, not a re-serialisation of the parsed
                // config: the shipped file is mostly comments explaining what
                // each setting does, and round-tripping through the serialiser
                // would hand the GUI a stripped copy to save back.
                let text = std::fs::read_to_string(&s.path).unwrap_or_else(|_| {
                    s.config.lock().unwrap().to_toml().unwrap_or_default()
                });
                (text, s.path.display().to_string())
            }
        };
        let set = {
            let s = shared.clone();
            move |text: String| -> Result<()> {
                let cfg: Config = toml::from_str(&text).context("parsing config")?;
                cfg.validate_against(&s.profile)?;
                // Only the button tables are regenerated, so the comments in a
                // hand-edited file survive a save from the GUI.
                std::fs::write(&s.path, &text)
                    .with_context(|| format!("writing {}", s.path.display()))?;
                *s.config.lock().unwrap() = cfg.clone();
                *s.pending_config.lock().unwrap() = Some(cfg);
                Ok(())
            }
        };
        let profile_src = {
            let s = shared.clone();
            move || {
                let text = std::fs::read_to_string(&s.profile_path)
                    .unwrap_or_else(|_| maschine_mk3::profile::BUILTIN_MK3.to_string());
                let where_ = if s.profile_path.exists() {
                    s.profile_path.display().to_string()
                } else {
                    "(built in)".to_string()
                };
                (text, where_)
            }
        };
        let presets = {
            let s = shared.clone();
            move |req: Request| -> Reply {
                let fail = |e: anyhow::Error| Reply::Error {
                    message: format!("{e:#}"),
                };
                match req {
                    Request::ListPresets => {
                        let active = s.config.lock().unwrap().preset.clone().map(|p| p.name);
                        Reply::Presets {
                            entries: preset::list()
                                .into_iter()
                                .map(|e| PresetEntry {
                                    name: e.name,
                                    description: e.description,
                                    builtin: e.origin == preset::Origin::Builtin,
                                    shadows_builtin: e.shadows_builtin,
                                })
                                .collect(),
                            dir: preset::dir().display().to_string(),
                            active,
                        }
                    }
                    Request::LoadPreset { name } => {
                        match preset::load_into(&name, &s.path, &s.profile) {
                            Ok(cfg) => {
                                *s.config.lock().unwrap() = cfg.clone();
                                *s.pending_config.lock().unwrap() = Some(cfg);
                                Reply::Ok
                            }
                            Err(e) => fail(e),
                        }
                    }
                    Request::SavePreset { name, description } => {
                        match preset::save_from(&name, &description, &s.path) {
                            Ok(_) => Reply::Ok,
                            Err(e) => fail(e),
                        }
                    }
                    Request::DeletePreset { name } => match preset::delete(&name) {
                        Ok(()) => Reply::Ok,
                        Err(e) => fail(e),
                    },
                    Request::ImportPreset { name, toml } => {
                        match preset::import(&name, &toml, &s.profile) {
                            Ok(_) => Reply::Ok,
                            Err(e) => fail(e),
                        }
                    }
                    _ => Reply::Error {
                        message: "unhandled request".into(),
                    },
                }
            }
        };
        std::thread::Builder::new()
            .name("mk3-ipc".into())
            .spawn(move || {
                if let Err(e) =
                    ipc::serve(&path, broadcaster, running, get, set, profile_src, presets)
                {
                    eprintln!("[ipc] stopped: {e:#}");
                }
            })?
    };

    let gui_thread = if cfg.general.gui_port != 0 {
        let shared = shared.clone();
        let bind = format!("{}:{}", cfg.general.gui_bind, cfg.general.gui_port);
        Some(
            std::thread::Builder::new()
                .name("mk3-gui".into())
                .spawn(move || {
                    let get = {
                        let s = shared.clone();
                        move || s.config.lock().unwrap().clone()
                    };
                    let set = {
                        let s = shared.clone();
                        move |c: Config| -> Result<()> {
                            c.save(&s.path)?;
                            *s.config.lock().unwrap() = c.clone();
                            *s.pending_config.lock().unwrap() = Some(c);
                            Ok(())
                        }
                    };
                    if let Err(e) = gui::serve(&bind, get, set, &shared.running) {
                        eprintln!("[gui] stopped: {e:#}");
                    }
                })?,
        )
    } else {
        None
    };

    // The core thread runs in this thread so that a panic here is fatal rather
    // than leaving a driver with no input.
    core_loop(shared.clone(), &mut hid_in, &midi, profile, cfg)?;

    shared.running.store(false, Ordering::SeqCst);
    let _ = surface.join();
    let _ = watcher.join();
    let _ = ipc_thread.join();
    if let Some(g) = gui_thread {
        let _ = g.join();
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn core_loop(
    shared: Arc<Shared>,
    hid: &mut HidDev,
    midi: &MidiIo,
    profile: Profile,
    cfg: Config,
) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    if cfg.general.realtime_priority > 0 {
        rt::try_realtime(cfg.general.realtime_priority, "core");
    }

    let mut engine = Engine::new(profile, cfg);
    let mut leds = Leds::new();
    engine.paint_idle(&mut leds);
    publish_leds(&shared, &mut leds);

    let mut buf = [0u8; 128];
    let mut hits: Vec<PadHit> = Vec::with_capacity(hid::PADS);
    let mut input = midi.input()?;

    let hid_fd = {
        // `HidDev` owns the file; borrow its descriptor for poll.
        let mut probe = libc::pollfd {
            fd: -1,
            events: libc::POLLIN,
            revents: 0,
        };
        probe.fd = hid.as_fd().as_raw_fd();
        probe
    };
    let seq_fds = midi.poll_fds()?;
    let mut fds: Vec<libc::pollfd> = std::iter::once(hid_fd).chain(seq_fds).collect();

    let mut last_reload_check = Instant::now();
    let mut prev_controls = ControlState::default();

    while !stopping(&shared) {
        for f in fds.iter_mut() {
            f.revents = 0;
        }
        // SAFETY: `fds` is a live, correctly sized array of pollfd.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 200) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("poll");
        }

        if fds[0].revents & libc::POLLIN != 0 {
            let r = hid.read_report(&mut buf)?;
            if !r.is_empty() {
                let mut send = |m: Msg| {
                    if let Err(e) = midi.send(m) {
                        eprintln!("[core] MIDI send failed: {e:#}");
                    }
                };
                match r[0] {
                    0x01 => {
                        if let Some(s) = ControlState::parse(&r[1..]) {
                            let before = *engine.outputs();
                            engine.on_controls(&s, &mut leds, &mut send);
                            let after = *engine.outputs();
                            if let Ok(mut slot) = shared.outputs.try_lock() {
                                *slot = after;
                            }
                            // Serialising costs more than the whole mapping
                            // step, so it only happens when someone is
                            // actually listening.
                            if shared.broadcast.has_clients() {
                                report_controls(&shared, &prev_controls, &s, &before, &after);
                            }
                            prev_controls = s;
                        }
                    }
                    0x02 => {
                        hid::parse_pads(&r[1..], &mut hits);
                        if !hits.is_empty() {
                            engine.on_pads(&hits, &mut leds, &mut send);
                            if shared.broadcast.has_clients() {
                                for h in &hits {
                                    let (down, value) = match h.event {
                                        hid::PadEvent::NoteOn | hid::PadEvent::PressOn => {
                                            (true, engine.config().pads.velocity(h.value))
                                        }
                                        hid::PadEvent::NoteOff | hid::PadEvent::PressOff => (false, 0),
                                        hid::PadEvent::Aftertouch => {
                                            (true, engine.config().pads.pressure(h.value))
                                        }
                                    };
                                    shared.broadcast.send(&IpcEvent::Pad {
                                        pad: h.pad,
                                        down,
                                        value,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
                publish_leds(&shared, &mut leds);
            }
        }

        if fds[1..].iter().any(|f| f.revents & libc::POLLIN != 0) {
            while let Ok(ev) = input.event_input() {
                if let Some(m) = from_alsa(&ev) {
                    engine.on_host_midi(m, &mut leds);
                }
                if input.event_input_pending(true).unwrap_or(0) == 0 {
                    break;
                }
            }
            publish_leds(&shared, &mut leds);
        }

        // Config reloads are checked off the hot path, at most five times a
        // second, and only ever swap a fully validated config in.
        if last_reload_check.elapsed() >= Duration::from_millis(200) {
            last_reload_check = Instant::now();
            // A publish that lost the `try_lock` race would otherwise sit
            // unsent until the next event; retry it on the slow path.
            publish_leds(&shared, &mut leds);
            let taken = shared.pending_config.lock().ok().and_then(|mut g| g.take());
            if let Some(new_cfg) = taken {
                eprintln!("[core] config reloaded");
                engine.reload(new_cfg);
                leds.all_off();
                engine.paint_idle(&mut leds);
                publish_leds(&shared, &mut leds);
            }
        }
    }
    Ok(())
}

/// Broadcast whatever changed in report `0x01`.
fn report_controls(
    shared: &Shared,
    prev: &ControlState,
    now: &ControlState,
    before: &maschine_mk3::engine::Outputs,
    after: &maschine_mk3::engine::Outputs,
) {
    for byte in 0..10 {
        let changed = now.buttons[byte] ^ prev.buttons[byte];
        let mut bits = changed;
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            shared.broadcast.send(&IpcEvent::Button {
                bit: byte * 8 + b,
                down: now.buttons[byte] & (1 << b) != 0,
            });
        }
    }
    for i in 0..hid::KNOBS {
        if after.knobs[i] != before.knobs[i] {
            shared.broadcast.send(&IpcEvent::Knob {
                knob: i,
                value: after.knobs[i],
            });
        }
    }
    if after.encoder != before.encoder {
        shared.broadcast.send(&IpcEvent::Encoder {
            value: after.encoder,
        });
    }
    if after.strip != before.strip {
        shared.broadcast.send(&IpcEvent::Strip { value: after.strip });
    }
}

fn publish_leds(shared: &Shared, leds: &mut Leds) {
    if !leds.is_dirty() {
        return;
    }
    if let Ok(mut slot) = shared.leds.try_lock() {
        for i in 0..LED_COUNT {
            slot[i] = leds.get(i);
        }
        shared
            .leds_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        leds.mark_published();
    }
}

fn from_alsa(ev: &alsa::seq::Event) -> Option<Msg> {
    use alsa::seq::EventType as T;
    match ev.get_type() {
        T::Noteon => ev.get_data::<alsa::seq::EvNote>().map(|n| Msg::NoteOn {
            ch: n.channel,
            note: n.note,
            vel: n.velocity,
        }),
        T::Noteoff => ev.get_data::<alsa::seq::EvNote>().map(|n| Msg::NoteOff {
            ch: n.channel,
            note: n.note,
            vel: n.off_velocity,
        }),
        T::Controller => ev.get_data::<alsa::seq::EvCtrl>().map(|c| Msg::Cc {
            ch: c.channel,
            cc: c.param as u8,
            val: c.value as u8,
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------

fn surface_thread(shared: Arc<Shared>, mut hid: HidDev) -> Result<()> {
    let cfg = shared.config.lock().unwrap().clone();

    hid.set_display_backlight(0, cfg.display.brightness, cfg.display.contrast)
        .ok();
    hid.set_display_backlight(1, cfg.display.brightness, cfg.display.contrast)
        .ok();

    let mut screens = match Displays::open() {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("[surface] screens unavailable ({e:#}); LEDs still active");
            None
        }
    };

    let mut left = Frame::new();
    let mut right = Frame::new();
    let mut surface = ui::Surface::new();

    if let Some(s) = screens.as_mut() {
        ui::splash(&mut left, "MASCHINE", "mk3d ready");
        ui::splash(&mut right, "MK3", "linux userspace driver");
        let _ = s.flush(0, &mut left);
        let _ = s.flush(1, &mut right);
        std::thread::sleep(Duration::from_millis(700));
        surface.invalidate();
    }

    let led_interval = Duration::from_micros(1_000_000 / cfg.leds.fps.clamp(1, 250) as u64);
    let disp_interval = Duration::from_micros(1_000_000 / cfg.display.fps.clamp(1, 60) as u64);
    let mut next_led = Instant::now();
    let mut next_disp = Instant::now();
    let mut sent_generation = u64::MAX;
    let mut mirror = Leds::new();

    while !stopping(&shared) {
        let now = Instant::now();

        if now >= next_led {
            next_led = now + led_interval;
            let generation = shared.leds_generation.load(Ordering::Acquire);
            if generation != sent_generation {
                if let Ok(slot) = shared.leds.try_lock() {
                    for i in 0..LED_COUNT {
                        mirror.set(i, slot[i]);
                    }
                    sent_generation = generation;
                }
                if let Err(e) = mirror.flush(&mut hid) {
                    eprintln!("[surface] LED write failed: {e:#}");
                }
            }
        }

        if now >= next_disp {
            next_disp = now + disp_interval;
            if let Some(s) = screens.as_mut() {
                let cfg = shared.config.lock().unwrap().clone();
                if cfg.display.enabled {
                    let out = *shared.outputs.lock().unwrap();
                    surface.update(&cfg, &out, &mut left, &mut right);
                    if let Err(e) = s.flush(0, &mut left).and_then(|_| s.flush(1, &mut right)) {
                        eprintln!("[surface] display write failed: {e:#}");
                    }
                }
            }
        }

        let sleep = next_led.min(next_disp).saturating_duration_since(Instant::now());
        std::thread::sleep(sleep.min(Duration::from_millis(20)).max(Duration::from_millis(1)));
    }

    mirror.all_off();
    let _ = mirror.flush(&mut hid);
    Ok(())
}

// ---------------------------------------------------------------------------

fn watch_thread(shared: Arc<Shared>) -> Result<()> {
    use notify::{RecursiveMode, Watcher};
    let dir = shared
        .path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    while !stopping(&shared) {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        let Ok(ev) = ev else { continue };
        if !ev.paths.iter().any(|p| p == &shared.path) {
            continue;
        }
        // Editors write in several steps; give the file a moment to settle.
        std::thread::sleep(Duration::from_millis(120));
        match Config::load(&shared.path) {
            Ok(c) => match c.validate_against(&shared.profile) {
                Ok(()) => {
                    *shared.config.lock().unwrap() = c.clone();
                    *shared.pending_config.lock().unwrap() = Some(c);
                }
                Err(e) => eprintln!("[watch] keeping the running config: {e:#}"),
            },
            Err(e) => eprintln!("[watch] keeping the running config: {e:#}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

/// Set from the signal handler; the core loop polls it alongside `Shared`.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    // SAFETY: `handler` only stores to an atomic, which is async-signal-safe;
    // nothing else in the handler can allocate, lock or reenter.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
    }
}

extern "C" fn handler(_sig: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// True once the process has been asked to stop, from either source.
fn stopping(shared: &Shared) -> bool {
    if SIGNALLED.load(Ordering::Relaxed) {
        shared.running.store(false, Ordering::SeqCst);
        return true;
    }
    !shared.running.load(Ordering::Relaxed)
}
