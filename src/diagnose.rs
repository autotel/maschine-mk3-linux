//! `mk3d --diagnose`: check everything between the device and the host.
//!
//! Most reports of "it doesn't work" are one of a handful of things, and all
//! of them are checkable: the device is not plugged in, udev has not been
//! reloaded, another process holds the display interface, or -- by far the
//! most common -- the host lists the MIDI port without ever subscribing to it,
//! which looks exactly like a driver that is not transmitting.
//!
//! Each check says what it found and, when something is wrong, the command
//! that fixes it. Nothing here changes anything.

use crate::{profile::Profile, PID, VID};
use std::fmt::Write as _;
use std::path::Path;

/// How a check came out.
enum Verdict {
    /// Working.
    Ok(String),
    /// Working, but worth knowing about.
    Warn(String, String),
    /// Broken, with what to do about it.
    Bad(String, String),
}

/// Run every check and print a report. Returns false if anything is broken.
pub fn run(config_path: &Path) -> bool {
    let mut bad = 0usize;
    let mut warn = 0usize;

    println!("Maschine MK3 -- diagnostics\n");
    for (title, verdict) in checks(config_path) {
        match verdict {
            Verdict::Ok(detail) => println!("  ok    {title}\n        {detail}"),
            Verdict::Warn(detail, fix) => {
                warn += 1;
                println!("  note  {title}\n        {detail}\n        {fix}");
            }
            Verdict::Bad(detail, fix) => {
                bad += 1;
                println!("  FAIL  {title}\n        {detail}\n        -> {fix}");
            }
        }
        println!();
    }

    if bad == 0 && warn == 0 {
        println!("Everything checks out.");
    } else {
        println!("{bad} problem(s), {warn} note(s).");
    }
    bad == 0
}

fn checks(config_path: &Path) -> Vec<(&'static str, Verdict)> {
    vec![
        ("USB device", usb()),
        ("HID access", hid()),
        ("Display interface", display()),
        ("Audio interface", audio()),
        ("Config and profile", files(config_path)),
        ("MIDI port", midi_port()),
        ("Who is listening", subscribers()),
        ("PipeWire / JACK", pipewire()),
        ("Real-time scheduling", realtime()),
    ]
}

fn usb() -> Verdict {
    // /sys is the authority; `lsusb` may not be installed.
    let found = std::fs::read_dir("/sys/bus/usb/devices")
        .map(|rd| {
            rd.flatten().any(|e| {
                let v = std::fs::read_to_string(e.path().join("idVendor")).unwrap_or_default();
                let p = std::fs::read_to_string(e.path().join("idProduct")).unwrap_or_default();
                v.trim().eq_ignore_ascii_case(&format!("{VID:04x}"))
                    && p.trim().eq_ignore_ascii_case(&format!("{PID:04x}"))
            })
        })
        .unwrap_or(false);
    if found {
        Verdict::Ok(format!("{VID:04x}:{PID:04x} is present"))
    } else {
        Verdict::Bad(
            format!("no {VID:04x}:{PID:04x} on the USB bus"),
            "plug the Maschine in. The driver waits, so it will pick it up on its own.".into(),
        )
    }
}

fn hid() -> Verdict {
    match crate::device::find_hidraw() {
        Err(e) => Verdict::Bad(
            e.to_string(),
            "if the device is plugged in, install udev/98-maschine-mk3.rules and replug".into(),
        ),
        Ok(path) => match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(_) => Verdict::Ok(format!("{} is readable and writable", path.display())),
            Err(e) => Verdict::Bad(
                format!("{} cannot be opened: {e}", path.display()),
                "sudo cp udev/98-maschine-mk3.rules /etc/udev/rules.d/ && \
                 sudo udevadm control --reload, then unplug and replug"
                    .into(),
            ),
        },
    }
}

fn display() -> Verdict {
    match crate::display::Displays::open() {
        Ok(_) => Verdict::Ok("USB interface 5 claimed; the screens will work".into()),
        Err(e) => {
            let msg = e.to_string();
            // Only one process can hold the interface, and a stray copy of the
            // driver or a learn tool is the usual reason.
            if msg.contains("Busy") || msg.contains("busy") {
                Verdict::Warn(
                    "USB interface 5 is held by something else".into(),
                    "another mk3d or a mk3-learn is running: pkill mk3d".into(),
                )
            } else {
                Verdict::Warn(
                    format!("cannot claim USB interface 5: {msg}"),
                    "LEDs and MIDI still work; only the screens are affected".into(),
                )
            }
        }
    }
}

fn audio() -> Verdict {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    if cards.contains("Maschine MK3") {
        Verdict::Ok(
            "the sound card is bound to snd-usb-audio: 4 out / 2 in, up to 96 kHz \
             (see audio/README.md)"
                .into(),
        )
    } else {
        Verdict::Warn(
            "the MK3 sound card is not in /proc/asound/cards".into(),
            "the control surface does not need it; check the cable if you wanted audio".into(),
        )
    }
}

fn files(config_path: &Path) -> Verdict {
    let profile_path = Profile::default_path();
    let profile = if profile_path.exists() {
        format!("profile: {}", profile_path.display())
    } else {
        "profile: built in".to_string()
    };
    if !config_path.exists() {
        return Verdict::Warn(
            format!("{} does not exist yet; {profile}", config_path.display()),
            "it is written on first run, or with: mk3d --write-default-config".into(),
        );
    }
    match crate::config::Config::load(config_path) {
        Err(e) => Verdict::Bad(
            format!("{e:#}"),
            "fix the file, or start again with: mk3d --preset default".into(),
        ),
        Ok(cfg) => match Profile::load_or_builtin(&profile_path) {
            Err(e) => Verdict::Bad(format!("{e:#}"), "correct the device profile".into()),
            Ok(p) => match cfg.validate_against(&p) {
                Err(e) => Verdict::Bad(
                    format!("{e:#}"),
                    "correct the name, or load a preset: mk3d --preset default".into(),
                ),
                Ok(()) => {
                    let bound = cfg
                        .buttons
                        .values()
                        .filter(|b| b.resolve().send != "none")
                        .count();
                    Verdict::Ok(format!(
                        "{} valid; {bound} of {} buttons send something; {profile}",
                        config_path.display(),
                        cfg.buttons.len()
                    ))
                }
            },
        },
    }
}

fn seq_clients() -> String {
    std::fs::read_to_string("/proc/asound/seq/clients").unwrap_or_default()
}

fn midi_port() -> Verdict {
    let clients = seq_clients();
    if clients.is_empty() {
        return Verdict::Bad(
            "the ALSA sequencer is not available".into(),
            "load it with: sudo modprobe snd-seq".into(),
        );
    }
    if clients.contains("Controller Out") {
        Verdict::Ok("the driver's output port exists".into())
    } else {
        Verdict::Warn(
            "no 'Controller Out' port; the driver may not be running".into(),
            "start it with: mk3d".into(),
        )
    }
}

fn subscribers() -> Verdict {
    let clients = seq_clients();
    // The port block lists its subscribers under "Connected To:". Nothing
    // there means the port exists and nobody is reading it -- the single most
    // common reason a host "sees the device but gets nothing".
    let mut in_ours = false;
    let mut connected: Vec<String> = Vec::new();
    for line in clients.lines() {
        if line.starts_with("Client ") {
            in_ours = false;
        }
        if line.contains("Controller Out") {
            in_ours = true;
            continue;
        }
        if in_ours {
            if let Some(rest) = line.trim().strip_prefix("Connecting To: ") {
                connected.push(rest.to_string());
            }
            if line.trim().starts_with("Port ") {
                in_ours = false;
            }
        }
    }
    if !clients.contains("Controller Out") {
        return Verdict::Warn("the driver is not running".into(), "start it with: mk3d".into());
    }
    if connected.is_empty() {
        Verdict::Warn(
            "nothing is subscribed to the output port".into(),
            "This is normal until a host connects. If your host lists the device \
             but receives nothing, it has not subscribed: add its name to \
             general.connect_to, or run `aconnect` once. See `mk3d --list-ports`."
                .into(),
        )
    } else {
        Verdict::Ok(format!("subscribed: {}", connected.join(", ")))
    }
}

fn pipewire() -> Verdict {
    let Ok(out) = std::process::Command::new("pw-link").arg("-o").output() else {
        return Verdict::Warn(
            "pw-link not found, so PipeWire could not be checked".into(),
            "only matters if your host uses JACK rather than ALSA".into(),
        );
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let ours: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("Maschine MK3") && l.contains("Controller Out"))
        .collect();
    if ours.is_empty() {
        Verdict::Warn(
            "the output port is not on the PipeWire graph".into(),
            "fine for an ALSA host. A JACK host will not see it: enable \
             WirePlumber's alsa.midi bridge."
                .into(),
        )
    } else {
        Verdict::Ok(format!(
            "bridged to JACK as \"{}\"\n        \
             A JACK host still has to be connected to it -- use qpwgraph, or:\n        \
             pw-link \"{}\" \"YOUR-HOST:MIDI Input 1\"",
            ours[0].trim(),
            ours[0].trim()
        ))
    }
}

fn realtime() -> Verdict {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid rlimit for the duration of the call.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut lim) };
    if rc != 0 {
        return Verdict::Warn("could not read the rtprio limit".into(), String::new());
    }
    if lim.rlim_cur >= 20 {
        Verdict::Ok(format!("rtprio limit is {}, so SCHED_FIFO is available", lim.rlim_cur))
    } else {
        let mut fix = String::new();
        let _ = write!(
            fix,
            "sudo usermod -aG audio \"$USER\", then log out and back in. \
             The driver runs without it; only scheduler jitter is affected."
        );
        Verdict::Warn(format!("rtprio limit is {}", lim.rlim_cur), fix)
    }
}
