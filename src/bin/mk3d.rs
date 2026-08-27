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
    controls: Mutex<ControlState>,
    /// Set by the watcher and the GUI; consumed by the core thread.
    pending_config: Mutex<Option<Config>>,
    /// Latest config, for the GUI to read and write.
    config: Mutex<Config>,
    /// Path the config came from.
    path: PathBuf,
    running: AtomicBool,
    /// Bumped whenever the core thread changes LED state.
    leds_generation: std::sync::atomic::AtomicU64,
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut write_default = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--config" => path = args.next().map(PathBuf::from),
            "--write-default-config" => write_default = true,
            "-h" | "--help" => {
                eprintln!(
                    "mk3d [-c CONFIG] [--write-default-config]\n\n\
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

    let shared = Arc::new(Shared {
        leds: Mutex::new([0; LED_COUNT]),
        controls: Mutex::new(ControlState::default()),
        pending_config: Mutex::new(None),
        config: Mutex::new(cfg.clone()),
        path: path.clone(),
        running: AtomicBool::new(true),
        leds_generation: std::sync::atomic::AtomicU64::new(0),
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
    core_loop(shared.clone(), &mut hid_in, &midi, cfg)?;

    shared.running.store(false, Ordering::SeqCst);
    let _ = surface.join();
    let _ = watcher.join();
    if let Some(g) = gui_thread {
        let _ = g.join();
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn core_loop(shared: Arc<Shared>, hid: &mut HidDev, midi: &MidiIo, cfg: Config) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    if cfg.general.realtime_priority > 0 {
        rt::try_realtime(cfg.general.realtime_priority, "core");
    }

    let mut engine = Engine::new(cfg);
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
                            engine.on_controls(&s, &mut leds, &mut send);
                            if let Ok(mut slot) = shared.controls.try_lock() {
                                *slot = s;
                            }
                        }
                    }
                    0x02 => {
                        hid::parse_pads(&r[1..], &mut hits);
                        if !hits.is_empty() {
                            engine.on_pads(&hits, &mut leds, &mut send);
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
                    let ctrls = *shared.controls.lock().unwrap();
                    surface.update(&cfg, &ctrls, &mut left, &mut right);
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
            Ok(c) => {
                *shared.config.lock().unwrap() = c.clone();
                *shared.pending_config.lock().unwrap() = Some(c);
            }
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
