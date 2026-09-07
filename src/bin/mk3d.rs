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
    let mut diagnose = false;
    let mut test_midi = false;
    let mut list_presets = false;
    let mut load_preset: Option<String> = None;
    let mut save_preset: Option<String> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => path = args.next().map(PathBuf::from),
            "--write-default-config" => write_default = true,
            "--list-ports" => list_ports = true,
            "--diagnose" => diagnose = true,
            "--test-midi" => test_midi = true,
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
                     \x20 --diagnose              check everything and say what to fix\n\
                     \x20 --test-midi             send known notes and CCs on every channel\n\
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

    if diagnose {
        let ok = maschine_mk3::diagnose::run(&path);
        std::process::exit(if ok { 0 } else { 1 });
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

    let midi = MidiIo::open(
        &cfg.general.client_name,
        &cfg.general.out_port,
        &cfg.general.in_port,
    )?;
    // Kept as plain numbers (not the `MidiIo` itself) so the ipc thread can
    // query subscriptions on its own transient sequencer connection without
    // touching the real-time thread's `Seq` handle from another thread.
    let midi_out_addr = midi.out_addr();
    let midi_in_addr = midi.in_addr();

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

    if let Err(e) = midi.watch_announcements() {
        eprintln!("[mk3d] cannot watch sequencer announcements ({e:#}); hosts that start \
                   later will not be connected automatically");
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

    if test_midi {
        return send_test_midi(&midi, &profile, &cfg);
    }

    if cfg.general.auto_connect {
        let mut seen = Vec::new();
        let done = midi.connect_to_all_hosts(&mut seen, &auto_connect_exclude(&profile, &cfg));
        if done.is_empty() {
            eprintln!("[mk3d] auto-connect: nothing to connect to yet; \
                       hosts that start later will be picked up");
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
                if let Err(e) = surface_thread(shared) {
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
                    Request::GetMidiStatus => Reply::MidiStatus {
                        out_listeners: maschine_mk3::midi::query_subscribers(midi_out_addr, true),
                        in_sources: maschine_mk3::midi::query_subscribers(midi_in_addr, false),
                    },
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
                    let backend = gui::Backend {
                        get: {
                            let s = shared.clone();
                            Box::new(move || {
                                let text = std::fs::read_to_string(&s.path).unwrap_or_default();
                                (text, s.path.display().to_string())
                            })
                        },
                        set: {
                            let s = shared.clone();
                            Box::new(move |text: String| -> Result<()> {
                                let cfg: Config =
                                    toml::from_str(&text).context("parsing config")?;
                                cfg.validate_against(&s.profile)?;
                                std::fs::write(&s.path, &text)
                                    .with_context(|| format!("writing {}", s.path.display()))?;
                                *s.config.lock().unwrap() = cfg.clone();
                                *s.pending_config.lock().unwrap() = Some(cfg);
                                Ok(())
                            })
                        },
                        device: {
                            let s = shared.clone();
                            Box::new(move || {
                                format!(
                                    "{} · {} controls",
                                    s.profile.device.name,
                                    s.profile.control.len()
                                )
                            })
                        },
                        presets: {
                            let s = shared.clone();
                            Box::new(move || {
                                let active =
                                    s.config.lock().unwrap().preset.clone().map(|p| p.name);
                                let list = preset::list()
                                    .into_iter()
                                    .map(|e| {
                                        (
                                            e.name,
                                            e.description,
                                            e.origin == preset::Origin::Builtin,
                                        )
                                    })
                                    .collect();
                                (list, active, preset::dir().display().to_string())
                            })
                        },
                        load_preset: {
                            let s = shared.clone();
                            Box::new(move |name: &str| -> Result<()> {
                                let cfg = preset::load_into(name, &s.path, &s.profile)?;
                                *s.config.lock().unwrap() = cfg.clone();
                                *s.pending_config.lock().unwrap() = Some(cfg);
                                Ok(())
                            })
                        },
                        save_preset: {
                            let s = shared.clone();
                            Box::new(move |name: &str, description: &str| -> Result<()> {
                                preset::save_from(name, description, &s.path)?;
                                Ok(())
                            })
                        },
                    };
                    if let Err(e) = gui::serve(&bind, backend, &shared.running) {
                        eprintln!("[gui] stopped: {e:#}");
                    }
                })?,
        )
    } else {
        None
    };

    // The core thread runs in this thread so that a panic here is fatal rather
    // than leaving a driver with no input.
    core_loop(shared.clone(), &midi, profile, cfg)?;

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

/// Block until the device appears, or the driver is asked to stop.
///
/// A controller is not always plugged in when its driver starts -- at login,
/// or after a reboot with the cable out -- and exiting in that case means
/// something has to notice and start it again. Waiting costs nothing and makes
/// the service and the configuration app work before the hardware is there.
fn wait_for_device(shared: &Shared, what: &str) -> Option<HidDev> {
    let mut announced = false;
    while !stopping(shared) {
        match HidDev::open() {
            Ok(d) => {
                if announced {
                    eprintln!("[{what}] device back: {}", d.path().display());
                } else {
                    eprintln!("[{what}] HID: {}", d.path().display());
                }
                return Some(d);
            }
            Err(e) => {
                if !announced {
                    announced = true;
                    eprintln!("[{what}] waiting for the Maschine MK3 ({e})");
                }
                std::thread::sleep(Duration::from_millis(700));
            }
        }
    }
    None
}

fn core_loop(shared: Arc<Shared>, midi: &MidiIo, profile: Profile, cfg: Config) -> Result<()> {
    if cfg.general.realtime_priority > 0 {
        rt::try_realtime(cfg.general.realtime_priority, "core");
    }
    let mut engine = Engine::new(profile, cfg.clone());
    let mut announce = AnnounceState {
        auto_connect: cfg.general.auto_connect,
        exclude: vec![
            engine.profile().device.name.clone(),
            cfg.general.client_name.clone(),
        ],
        connected: Vec::new(),
        subscribers: 0,
    };

    // Each pass owns one connection. Losing the device drops out of the inner
    // loop and waits for it to come back, rather than taking the driver down.
    while !stopping(&shared) {
        let Some(mut hid) = wait_for_device(&shared, "core") else {
            break;
        };
        // A reconnected device has all its LEDs dark and no idea what was
        // held, so both have to be re-established.
        engine.forget_device_state();
        match core_session(&shared, &mut hid, midi, &mut engine, &mut announce) {
            Ok(()) => {}
            Err(e) if maschine_mk3::device::is_disconnect(&e) => {
                eprintln!("[core] device disconnected; waiting for it to come back");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn core_session(
    shared: &Arc<Shared>,
    hid: &mut HidDev,
    midi: &MidiIo,
    engine: &mut Engine,
    announce: &mut AnnounceState,
) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    let mut leds = Leds::new();
    engine.paint_idle(&mut leds);
    publish_leds(shared, &mut leds);

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

    while !stopping(shared) {
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
                                report_controls(shared, &prev_controls, &s, &before, &after);
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
                publish_leds(shared, &mut leds);
            }
        }

        if fds[1..].iter().any(|f| f.revents & libc::POLLIN != 0) {
            while let Ok(ev) = input.event_input() {
                if let Some(m) = from_alsa(&ev) {
                    engine.on_host_midi(m, &mut leds);
                } else {
                    // Announcements share the input port with host MIDI.
                    // Noting them here is what turns "nothing is happening"
                    // into a line saying who connected and when.
                    report_announcement(midi, &ev, announce);
                }
                if input.event_input_pending(true).unwrap_or(0) == 0 {
                    break;
                }
            }
            publish_leds(shared, &mut leds);
        }

        // Config reloads are checked off the hot path, at most five times a
        // second, and only ever swap a fully validated config in.
        if last_reload_check.elapsed() >= Duration::from_millis(200) {
            last_reload_check = Instant::now();
            // A publish that lost the `try_lock` race would otherwise sit
            // unsent until the next event; retry it on the slow path.
            publish_leds(shared, &mut leds);
            let taken = shared.pending_config.lock().ok().and_then(|mut g| g.take());
            if let Some(new_cfg) = taken {
                eprintln!("[core] config reloaded");
                engine.reload(new_cfg);
                leds.all_off();
                engine.paint_idle(&mut leds);
                publish_leds(shared, &mut leds);
            }
        }
    }
    Ok(())
}

/// Send a known pattern on every channel, so a silent host can be narrowed down.
///
/// When a host lists the device and receives nothing, the question is whether
/// the driver is sending, whether anything is subscribed, or whether the host
/// is filtering by channel. Pressing a pad answers none of those on its own;
/// this does, because what is sent is known in advance and covers every
/// channel.
fn send_test_midi(midi: &MidiIo, profile: &Profile, cfg: &Config) -> Result<()> {
    if cfg.general.auto_connect {
        let mut seen = Vec::new();
        for name in midi.connect_to_all_hosts(&mut seen, &auto_connect_exclude(profile, cfg)) {
            eprintln!("connected output to {name}");
        }
    }
    if !cfg.general.connect_to.is_empty() {
        for name in midi.connect_to_matching(&cfg.general.connect_to) {
            eprintln!("connected output to {name}");
        }
    }

    let dests = midi.destinations();
    println!(
        "Sending on all 16 channels. {} possible destination(s) exist.\n",
        dests.len()
    );
    println!(
        "In your host, set the track's MIDI input to *all channels* (or \"Omni\") \n\
         and arm it. Then watch which of these arrive:\n"
    );
    println!("  {:>3}  {:<26} {}", "ch", "note", "cc");
    for ch in 0..16u8 {
        let note = 60 + ch;
        let cc = 20 + ch;
        println!("  {:>3}  note {note} on/off{:<12} cc {cc} = 127 then 0", ch + 1, "");
        midi.send(Msg::NoteOn { ch, note, vel: 100 })?;
        std::thread::sleep(Duration::from_millis(120));
        midi.send(Msg::NoteOff { ch, note, vel: 0 })?;
        midi.send(Msg::Cc { ch, cc, val: 127 })?;
        std::thread::sleep(Duration::from_millis(60));
        midi.send(Msg::Cc { ch, cc, val: 0 })?;
        std::thread::sleep(Duration::from_millis(120));
    }

    println!(
        "\nDone.\n\n\
         If NOTHING arrived:\n\
         \x20 the host is not subscribed. Run `mk3d --list-ports`, then either set\n\
         \x20 general.connect_to = [\"YOUR HOST\"], or connect it once with aconnect.\n\
         \x20 If the host uses JACK rather than ALSA it is on a different graph --\n\
         \x20 see `mk3d --diagnose` for the exact port name to link.\n\n\
         If SOME channels arrived:\n\
         \x20 the host is filtering by channel. This preset puts pads on channel {},\n\
         \x20 knobs on {} and buttons on 16, so a track listening only to channel 1\n\
         \x20 hears the knobs and nothing else. Set the input to all channels.\n\n\
         If EVERYTHING arrived:\n\
         \x20 the routing is fine. If pressing pads still does nothing, the track is\n\
         \x20 probably not record-armed or input monitoring is off.",
        cfg.pads.channel, cfg.knobs.channel
    );
    Ok(())
}

/// Client names auto-connect must leave alone.
fn auto_connect_exclude(profile: &Profile, cfg: &Config) -> Vec<String> {
    vec![
        profile.device.name.clone(),
        cfg.general.client_name.clone(),
    ]
}

/// What the driver knows about who is listening.
struct AnnounceState {
    /// Whether to subscribe hosts as they appear.
    auto_connect: bool,
    /// Client names never connected automatically: ours, and the hardware's
    /// own DIN MIDI port.
    exclude: Vec<String>,
    /// Destinations already dealt with, so a rescan is quiet.
    connected: Vec<(i32, i32)>,
    /// How many subscriptions our output currently has.
    subscribers: usize,
}

/// Act on a sequencer announcement.
///
/// New ports get connected when `auto_connect` is on; subscriptions to our
/// output are logged either way, because "who is listening" is the question
/// nobody can answer from outside.
fn report_announcement(midi: &MidiIo, ev: &alsa::seq::Event, state: &mut AnnounceState) {
    use alsa::seq::EventType as T;
    match ev.get_type() {
        T::PortStart | T::ClientStart => {
            if state.auto_connect {
                for name in midi.connect_to_all_hosts(&mut state.connected, &state.exclude) {
                    eprintln!("[midi] connected output to {name}");
                }
            }
        }
        T::PortExit | T::ClientExit => {
            // Forget it, so the same client reconnecting is picked up again.
            if let Some(c) = ev.get_data::<alsa::seq::Connect>() {
                state.connected.retain(|&(cl, p)| {
                    (cl, p) != (c.sender.client, c.sender.port)
                        && (cl, p) != (c.dest.client, c.dest.port)
                });
            }
            state.connected.clear();
        }
        T::PortSubscribed | T::PortUnsubscribed => {
            let Some(c) = ev.get_data::<alsa::seq::Connect>() else {
                return;
            };
            let ours = c.sender.client == midi.client_id() && c.sender.port == midi.out_port();
            if !ours {
                return;
            }
            let who = midi.describe(c.dest);
            if ev.get_type() == T::PortSubscribed {
                state.subscribers += 1;
                eprintln!("[midi] {who} is now listening ({} subscriber(s))", state.subscribers);
            } else {
                state.subscribers = state.subscribers.saturating_sub(1);
                eprintln!("[midi] {who} stopped listening ({} left)", state.subscribers);
            }
        }
        _ => {}
    }
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

fn surface_thread(shared: Arc<Shared>) -> Result<()> {
    // The surface finds the device for itself rather than being handed one, so
    // it can reconnect independently of the input thread. Nothing it does is
    // urgent, so a lost device costs a repaint rather than a note.
    while !stopping(&shared) {
        let Some(hid) = wait_for_device(&shared, "surface") else {
            break;
        };
        match surface_session(&shared, hid) {
            Ok(()) => {}
            Err(e) if maschine_mk3::device::is_disconnect(&e) => {
                eprintln!("[surface] device disconnected");
            }
            Err(e) => eprintln!("[surface] {e:#}"),
        }
    }
    Ok(())
}

fn surface_session(shared: &Arc<Shared>, mut hid: HidDev) -> Result<()> {
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
    // u64::MAX can never match a real generation, so the first pass always
    // repaints -- which is what a just-connected device needs.
    let mut sent_generation = u64::MAX;
    let mut mirror = Leds::new();

    while !stopping(shared) {
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
                // A write failure here is how the surface learns the device
                // has gone; the input thread finds out separately.
                mirror.flush(&mut hid)?;
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
                        // The screens are on a separate USB interface, so
                        // losing them does not mean the device is gone.
                        screens = None;
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
