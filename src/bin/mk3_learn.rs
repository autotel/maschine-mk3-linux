//! Interactive hardware discovery for the Maschine MK3.
//!
//! The HID report descriptor tells us there are 80 button bits and 103 LED
//! slots, but not which physical control sits at which index -- that is only
//! discoverable by pressing things and watching what lights up. This tool does
//! that and writes the answers into a config file.

use anyhow::{bail, Context, Result};
use maschine_mk3::config::Config;
use maschine_mk3::device::HidDev;
use maschine_mk3::display::{self, font, Displays, Frame};
use maschine_mk3::hid::{self, ControlState};
use maschine_mk3::leds::{self, Leds, Level, LED_COUNT};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = args.collect();

    match cmd.as_str() {
        "watch" => watch(&rest),
        "buttons" => buttons(&rest),
        "leds" => walk_leds(&rest),
        "find-pads" => find_pads(&rest),
        "probe" => probe(&rest),
        "info" => info(),
        "test-display" => test_display(),
        "palette" => palette(),
        _ => {
            help();
            Ok(())
        }
    }
}

fn help() {
    eprintln!(
        "\
mk3-learn -- hardware discovery for the Maschine MK3

  watch [raw]            print every HID event as it arrives; `raw` also
                         hexdumps report 0x01 whenever any byte changes, which
                         catches controls that fall outside the known fields
  buttons [config] [--debug]
                         press each button in turn; records bit indices
  leds [start] [end]     light LED slots one at a time to identify them
  find-pads [config]     locate the pad LED block and record it in the config
  probe A B [VALUE]      light LED slots A..B at once (VALUE defaults to 0x47)
  probe rgb              light only the colour LEDs, leaving mono ones dark
  probe mono             light only the monochrome LEDs
  info                   dump the device's feature reports
  test-display           draw a test pattern on both screens
  palette                print the device's built-in colour palette
"
    );
}

fn open() -> Result<HidDev> {
    let dev = HidDev::open()?;
    eprintln!("[mk3] using {}", dev.path().display());
    Ok(dev)
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

// ---------------------------------------------------------------------------

fn watch(args: &[String]) -> Result<()> {
    let raw = args.iter().any(|a| a == "raw");
    let mut dev = open()?;
    let mut buf = [0u8; 128];
    let mut prev: Option<ControlState> = None;
    let mut prev_raw: Vec<u8> = Vec::new();
    let mut hits = Vec::new();
    eprintln!("[mk3] watching -- press anything, ctrl-c to stop");
    if raw {
        eprintln!("[mk3] raw mode: every changed report 0x01 is dumped in full");
    }

    loop {
        let Some(r) = dev.read_report_timeout(&mut buf, 500)? else {
            continue;
        };
        if r.is_empty() {
            continue;
        }
        match r[0] {
            0x01 => {
                if raw && r != prev_raw.as_slice() {
                    println!("raw 0x01     {}", hexdump(&r[1..]));
                    prev_raw = r.to_vec();
                }
                let Some(s) = ControlState::parse(&r[1..]) else {
                    continue;
                };
                if let Some(p) = prev {
                    for bit in 0..hid::BUTTON_BITS {
                        if s.button(bit) != p.button(bit) {
                            println!(
                                "button bit {bit:>2}  {}",
                                if s.button(bit) { "down" } else { "up" }
                            );
                        }
                    }
                    if s.encoder_lo != p.encoder_lo {
                        println!(
                            "encoder      {} -> {}  ({:+})",
                            p.encoder_lo,
                            s.encoder_lo,
                            hid::nibble_delta(p.encoder_lo, s.encoder_lo)
                        );
                    }
                    if s.encoder_hi != p.encoder_hi {
                        println!("encoder-hi   {} -> {}", p.encoder_hi, s.encoder_hi);
                    }
                    for i in 0..hid::KNOBS {
                        if s.knobs[i] != p.knobs[i] {
                            println!("knob {i}       {:>4} -> {:>4}", p.knobs[i], s.knobs[i]);
                        }
                    }
                    for i in 0..hid::ANALOGS {
                        if s.analog[i] != p.analog[i] {
                            let note = match i {
                                0 => "  (free-running counter, not a control)",
                                1 => "  (touch strip)",
                                _ => "",
                            };
                            println!(
                                "analog {i}     {:>5} -> {:>5}{note}",
                                p.analog[i], s.analog[i]
                            );
                        }
                    }
                } else {
                    println!("baseline: buttons={:02x?} knobs={:?}", s.buttons, s.knobs);
                }
                prev = Some(s);
            }
            0x02 => {
                hid::parse_pads(&r[1..], &mut hits);
                for h in &hits {
                    println!("pad {:>2}       {:?} @ {}", h.pad, h.event, h.value);
                }
            }
            other => println!("report 0x{other:02x}: {:02x?}", &r[1..]),
        }
    }
}

// ---------------------------------------------------------------------------

/// Walk the user through pressing every control, recording bit indices.
///
/// Two things make this harder than watching for a rising edge:
///
/// * A button still held when the next round begins has no edge to detect, so
///   its next press gets attributed to whatever else changed. Each round
///   therefore waits for the field to go quiet and takes the first non-zero
///   sample, rather than diffing against a reference that can go stale.
/// * The 4-D encoder reports its touch sensor a few milliseconds *before* the
///   direction being tilted, so the first non-zero sample holds only the touch
///   bit and the directions are unreachable. Activity is accumulated over a
///   short window instead of being read from one report.
///
/// Stdin is polled alongside the device, so `done` works while the tool is
/// waiting for a press rather than only at the name prompt.
fn buttons(args: &[String]) -> Result<()> {
    let debug = args.iter().any(|a| a == "--debug");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(Config::default_path);
    let mut cfg = if path.exists() {
        Config::load(&path)?
    } else {
        Config::default()
    };

    let mut dev = open()?;
    let mut buf = [0u8; 128];
    let mut ignored: Vec<usize> = Vec::new();

    println!("{BUTTONS_HELP}\nConfig: {}\n", path.display());

    'session: loop {
        // Wait for everything to be released, so the next press starts from a
        // known-quiet state. Ignored bits do not count as activity.
        let settle = Instant::now();
        let mut warned = false;
        loop {
            match poll_input(&mut dev, &mut buf, 200)? {
                Event::Line(line) => match handle_command(&line, &cfg, &mut ignored) {
                    Command::Done => break 'session,
                    Command::Handled => continue,
                    Command::NotACommand => {
                        println!("  (nothing to name yet -- press a button first)");
                    }
                },
                Event::Timeout => break,
                Event::Report(s) => {
                    let down = live_bits(&s, &ignored);
                    if down.is_empty() {
                        break;
                    }
                    if !warned && settle.elapsed() > Duration::from_millis(1500) {
                        warned = true;
                        println!(
                            "  ...still held: {down:?}. If one of these is stuck, \
                             type `ignore <bit>`."
                        );
                    }
                }
            }
        }

        // Collect everything that goes down over a short window. The encoder's
        // touch sensor leads its direction bits, so reading a single report
        // would only ever see the touch.
        let mut down: Vec<usize> = Vec::new();
        let mut window: Option<Instant> = None;
        loop {
            let remaining = match window {
                None => 200,
                Some(t) => {
                    let elapsed = t.elapsed();
                    if elapsed >= PRESS_WINDOW {
                        break;
                    }
                    (PRESS_WINDOW - elapsed).as_millis() as i32
                }
            };
            match poll_input(&mut dev, &mut buf, remaining.max(1))? {
                Event::Line(line) => match handle_command(&line, &cfg, &mut ignored) {
                    Command::Done => break 'session,
                    Command::Handled => continue,
                    Command::NotACommand => {
                        println!("  (nothing to name yet -- press a button first)");
                    }
                },
                Event::Timeout => {
                    if window.is_some() {
                        break;
                    }
                }
                Event::Report(s) => {
                    if debug {
                        println!("  debug: buttons={:02x?}", s.buttons);
                    }
                    for b in live_bits(&s, &ignored) {
                        if !down.contains(&b) {
                            down.push(b);
                        }
                    }
                    if !down.is_empty() && window.is_none() {
                        window = Some(Instant::now());
                    }
                }
            }
        }
        if down.is_empty() {
            continue;
        }
        down.sort_unstable();

        let named = |b: usize, cfg: &Config| -> Option<String> {
            cfg.button
                .iter()
                .find(|(_, c)| c.bit == b)
                .map(|(n, _)| n.clone())
        };

        // Prefer a bit nobody has claimed: with the encoder, the touch sensor
        // is already named by the time the directions are being learned, so
        // this picks the direction.
        let mut bit = down[0];
        if down.len() > 1 {
            let described: Vec<String> = down
                .iter()
                .map(|&b| match named(b, &cfg) {
                    Some(n) => format!("{b} ({n})"),
                    None => format!("{b} (new)"),
                })
                .collect();
            println!("  bits down: {}", described.join(", "));
            if let Some(u) = down.iter().copied().find(|&b| named(b, &cfg).is_none()) {
                bit = u;
            }
        }

        match named(bit, &cfg) {
            Some(n) => println!("bit {bit:>2} -- already mapped to `{n}`"),
            None => println!("bit {bit:>2} -- new"),
        }

        let line = prompt("  name> ")?;
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else {
            println!("  skipped");
            continue;
        };

        match handle_command(&line, &cfg, &mut ignored) {
            Command::Done => break 'session,
            Command::Handled => continue,
            Command::NotACommand => {}
        }

        // A bare number picks a different bit out of the ones just seen.
        if let Ok(n) = first.parse::<usize>() {
            if !down.contains(&n) {
                eprintln!("  ! bit {n} was not among {down:?}");
                continue;
            }
            bit = n;
            let rest: Vec<&str> = words.collect();
            let Some(name) = rest.first() else {
                println!("  using bit {n}; press the button again to name it");
                continue;
            };
            record(&mut cfg, &path, bit, name, rest.get(1).copied())?;
            continue;
        }

        record(&mut cfg, &path, bit, first, words.next())?;
    }

    Config::write_buttons_preserving(&path, &cfg.button)?;
    print_buttons(&cfg);
    println!("wrote {}", path.display());
    Ok(())
}

/// How long to keep collecting after the first bit goes down.
///
/// The encoder's touch sensor leads its direction bits by a few milliseconds;
/// this has to outlast that gap without being long enough to merge two
/// deliberate presses.
const PRESS_WINDOW: Duration = Duration::from_millis(350);

const BUTTONS_HELP: &str = "\
Press one button at a time, then say what it is.

  <name>            record it, e.g.  play
  <name> <led>      record it with an LED slot, e.g.  play 21
  <enter>           skip this press and wait for the next
  <bit> <name>      name a specific bit, when several were down at once
  ignore <bit>      stop treating that bit as activity (for a sticky sensor)
  unignore <bit>    undo that
  list              show what has been mapped so far
  help              show this again
  done              finish

`done`, `list` and `ignore` work at any time, not only at the name prompt.
Every name is written to the config the moment you enter it, so stopping with
ctrl-c keeps everything mapped so far, and re-running resumes where you left
off. Comments elsewhere in the file are left untouched.";

/// What arrived first: a device report, a typed line, or neither.
enum Event {
    Report(ControlState),
    Line(String),
    Timeout,
}

/// Outcome of interpreting a typed line as a command.
enum Command {
    Done,
    Handled,
    NotACommand,
}

fn handle_command(line: &str, cfg: &Config, ignored: &mut Vec<usize>) -> Command {
    let mut w = line.split_whitespace();
    let Some(head) = w.next() else {
        return Command::NotACommand;
    };
    match head {
        "done" | "quit" | "exit" => Command::Done,
        "list" => {
            print_buttons(cfg);
            if !ignored.is_empty() {
                println!("  ignoring bits {ignored:?}");
            }
            Command::Handled
        }
        "help" | "?" => {
            println!("{BUTTONS_HELP}");
            Command::Handled
        }
        "ignore" | "unignore" => {
            let Some(n) = w.next().and_then(|t| t.parse::<usize>().ok()) else {
                eprintln!("  ! usage: {head} <bit>");
                return Command::Handled;
            };
            if n >= hid::BUTTON_BITS {
                eprintln!("  ! bit {n} is outside 0..{}", hid::BUTTON_BITS);
                return Command::Handled;
            }
            if head == "ignore" {
                if !ignored.contains(&n) {
                    ignored.push(n);
                    ignored.sort_unstable();
                }
                println!("  ignoring bit {n}");
            } else {
                ignored.retain(|&b| b != n);
                println!("  no longer ignoring bit {n}");
            }
            Command::Handled
        }
        _ => Command::NotACommand,
    }
}

/// Record one button and persist immediately.
fn record(
    cfg: &mut Config,
    path: &Path,
    bit: usize,
    name: &str,
    led: Option<&str>,
) -> Result<()> {
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        eprintln!("  ! names may only contain letters, digits, `_` and `-`");
        return Ok(());
    }
    let led = match led {
        None => None,
        Some(t) => match t.parse::<i32>() {
            Ok(v) if v >= -1 && (v as usize) < LED_COUNT => Some(v),
            _ => {
                eprintln!("  ! LED slot must be -1 or 0..{}; nothing recorded", LED_COUNT - 1);
                return Ok(());
            }
        },
    };

    // Reusing a name would silently move it off the button it already
    // describes, leaving that button unmapped and the config quietly wrong.
    if let Some(existing) = cfg.button.get(name) {
        if existing.bit != bit {
            eprintln!(
                "  ! `{name}` already means bit {}; pick another name or rename that one first",
                existing.bit
            );
            return Ok(());
        }
    }
    // A bit can only belong to one name, so retire whatever held it.
    let old = cfg
        .button
        .iter()
        .find(|(_, c)| c.bit == bit)
        .map(|(n, _)| n.clone());
    if let Some(old) = old {
        if old != name {
            cfg.button.remove(&old);
            println!("  (renamed `{old}` to `{name}`)");
        }
    }

    let entry = cfg.button.entry(name.to_string()).or_default();
    entry.bit = bit;
    if let Some(v) = led {
        entry.led = v;
    }

    // Persist after every single button. A discovery session is long and easy
    // to interrupt; losing it to a stray ctrl-c would be worse than the cost
    // of a small write each time.
    match Config::write_buttons_preserving(path, &cfg.button) {
        Ok(()) => match led {
            Some(v) => println!("  ok: button.{name} = bit {bit}, led {v}  [saved]"),
            None => println!("  ok: button.{name} = bit {bit}  [saved]"),
        },
        Err(e) => eprintln!("  ! could not save: {e:#}"),
    }
    Ok(())
}

/// Wait for either a device report or a line on stdin.
fn poll_input(dev: &mut HidDev, buf: &mut [u8; 128], timeout_ms: i32) -> Result<Event> {
    use std::os::unix::io::{AsFd, AsRawFd};
    let mut fds = [
        libc::pollfd {
            fd: dev.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: two valid pollfds owned by this call.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(Event::Timeout);
        }
        return Err(e).context("poll");
    }
    if rc == 0 {
        return Ok(Event::Timeout);
    }
    if fds[1].revents & libc::POLLIN != 0 {
        let mut line = String::new();
        // The terminal is line buffered, so POLLIN means a whole line is ready.
        if std::io::stdin().lock().read_line(&mut line)? == 0 {
            return Ok(Event::Line("done".into())); // stdin closed
        }
        return Ok(Event::Line(line.trim().to_string()));
    }
    if fds[0].revents & libc::POLLIN != 0 {
        if let Some(r) = dev.read_report_timeout(buf, 0)? {
            if r.len() >= 2 && r[0] == 0x01 {
                if let Some(s) = ControlState::parse(&r[1..]) {
                    return Ok(Event::Report(s));
                }
            }
        }
    }
    Ok(Event::Timeout)
}

/// Button bits currently held, minus any the user asked to ignore.
fn live_bits(s: &ControlState, ignored: &[usize]) -> Vec<usize> {
    (0..hid::BUTTON_BITS)
        .filter(|&b| s.button(b) && !ignored.contains(&b))
        .collect()
}

fn print_buttons(cfg: &Config) {
    if cfg.button.is_empty() {
        println!("  (nothing mapped yet)");
        return;
    }
    let mut rows: Vec<(&String, &maschine_mk3::config::ButtonCfg)> = cfg.button.iter().collect();
    rows.sort_by_key(|(_, b)| b.bit);
    println!("  {:>4}  {:>4}  {}", "bit", "led", "name");
    for (name, b) in rows {
        println!("  {:>4}  {:>4}  {}", b.bit, b.led, name);
    }
}

// ---------------------------------------------------------------------------

/// Light LED slots one at a time so the user can record what each one is.
fn walk_leds(args: &[String]) -> Result<()> {
    let start: usize = args.first().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let end: usize = args
        .get(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(LED_COUNT)
        .min(LED_COUNT);
    if start >= end {
        bail!("start {start} must be below end {end}");
    }

    let mut dev = open()?;
    let mut leds = Leds::new();
    leds.all_off();
    leds.flush(&mut dev)?;

    println!(
        "\
Lighting LED slots {start}..{end}, one at a time. Type what you see lit and
press enter to advance; empty input also advances. Type `q` to stop, `b` to
step back. Slots 0..{} are report 0x80, the rest are report 0x81.
",
        leds::BANK0_LEN
    );

    let mut i = start;
    while i < end {
        leds.all_off();
        // Mono LEDs read the byte as brightness; colour LEDs read it as
        // (palette << 2) | level. 0x7f lights either kind, which is what we
        // want when we do not yet know which this slot is.
        leds.raw_mut()[i] = 0x7f;
        leds.flush(&mut dev)?;

        let ans = prompt(&format!("slot {i:>3}> "))?;
        match ans.as_str() {
            "q" => break,
            "b" => i = i.saturating_sub(1),
            "" => i += 1,
            label => {
                println!("  slot {i} = {label}");
                i += 1;
            }
        }
    }

    leds.all_off();
    leds.flush(&mut dev)?;
    Ok(())
}

// ---------------------------------------------------------------------------

fn info() -> Result<()> {
    let mut dev = open()?;
    // Report id -> payload length, from the device's report descriptor.
    let reports: &[(u8, usize, &str)] = &[
        (0xd0, 32, "device configuration"),
        (0xd8, 32, "hardware identity"),
        (0xd9, 32, "serial"),
        (0xda, 32, "pad calibration: baselines"),
        (0xdb, 32, "pad calibration: ranges"),
        (0xf0, 11, "sensitivity"),
        (0xf8, 10, "display 0"),
        (0xf9, 10, "display 1"),
    ];
    for &(rid, len, what) in reports {
        let mut buf = vec![0u8; len + 1];
        buf[0] = rid;
        match dev.get_feature(&mut buf) {
            Ok(r) => {
                println!("0x{rid:02x} {what}");
                println!("     {}", hexdump(&r[1..]));
                if rid == 0xf8 || rid == 0xf9 {
                    let w = u16::from_le_bytes([r[1], r[2]]);
                    let h = u16::from_le_bytes([r[3], r[4]]);
                    println!(
                        "     {w}x{h}, {} bpp, brightness {}, contrast {}",
                        r[5], r[8], r[9]
                    );
                }
                if rid == 0xda || rid == 0xdb {
                    let v: Vec<u16> = r[1..]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    println!("     {v:?}");
                }
                if rid == 0xd9 {
                    let s: String = r[1..]
                        .iter()
                        .take_while(|&&c| c != 0)
                        .map(|&c| c as char)
                        .collect();
                    println!("     \"{s}\"");
                }
            }
            Err(e) => println!("0x{rid:02x} {what}: {e}"),
        }
    }
    Ok(())
}

fn hexdump(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn palette() -> Result<()> {
    let mut dev = open()?;
    for rid in [0xfeu8, 0xff] {
        println!("palette 0x{rid:02x}");
        let ramps = leds::read_palette(&mut dev, rid)?;
        for (i, r) in ramps.iter().enumerate() {
            let cells: Vec<String> = r
                .0
                .iter()
                .map(|(r, g, b)| format!("({r:>3},{g:>3},{b:>3})"))
                .collect();
            println!("  {i:>2}: {}", cells.join(" "));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn test_display() -> Result<()> {
    let mut hid = open()?;
    let mut screens = Displays::open().context(
        "claiming USB interface 5 -- install udev/98-maschine-mk3.rules and replug the device",
    )?;

    hid.set_display_backlight(0, 100, 50).ok();
    hid.set_display_backlight(1, 100, 50).ok();

    let start = Instant::now();
    for (idx, label) in [(0u8, "LEFT"), (1, "RIGHT")] {
        let mut f = Frame::new();
        // Colour ramp: a wrong byte order or stride shows up immediately here.
        for y in 0..display::H {
            for x in 0..display::W {
                let r = (x * 255 / display::W) as u8;
                let g = (y * 255 / display::H) as u8;
                let b = 255u8.saturating_sub(r / 2 + g / 2);
                f.put(x, y, display::rgb(r, g, b));
            }
        }
        f.rect(0, 0, display::W, 40, display::rgb(0, 0, 0));
        f.text(12, 10, &font::LARGE, 1, display::WHITE, label);
        f.frame_rect(0, 0, display::W, display::H, display::WHITE);
        screens.blit(idx, 0, 0, display::W, display::H, f.pixels_mut())?;
    }
    println!(
        "pushed two {}x{} frames in {:?}",
        display::W,
        display::H,
        start.elapsed()
    );
    println!("Both screens should now show a colour gradient labelled LEFT and RIGHT.");

    // Also prove the LED path: a slow sweep across every slot.
    let mut leds = Leds::new();
    println!("sweeping all {LED_COUNT} LED slots...");
    for i in 0..LED_COUNT {
        leds.all_off();
        leds.raw_mut()[i] = leds::colour(17, Level::Bright);
        leds.flush(&mut hid)?;
        std::thread::sleep(Duration::from_millis(25));
    }
    leds.all_off();
    leds.flush(&mut hid)?;
    println!("done");
    Ok(())
}

// ---------------------------------------------------------------------------

/// Light every slot in `range` at once and leave it lit.
///
/// The value matters: a monochrome LED reads its byte as brightness across
/// 0..=127, while a colour LED reads it as `(palette << 2) | level` and treats
/// level 0 as off. So `0x7c` (palette 31, level 0) lights only monochrome LEDs
/// and `0x05` (palette 1, level 1) lights only colour ones -- which is enough
/// to tell the two kinds apart by eye.
fn probe(args: &[String]) -> Result<()> {
    let (lo, hi, value) = match args.first().map(String::as_str) {
        Some("rgb") => (0, LED_COUNT, 0x05),
        Some("mono") => (0, LED_COUNT, 0x7c),
        _ => {
            let lo: usize = args.first().map(|s| s.parse()).transpose()?.unwrap_or(0);
            let hi: usize = args
                .get(1)
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(LED_COUNT);
            let v = match args.get(2) {
                Some(s) => parse_u8(s)?,
                None => 0x47,
            };
            (lo, hi, v)
        }
    };
    let hi = hi.min(LED_COUNT);
    if lo >= hi {
        bail!("empty range {lo}..{hi}");
    }

    let mut dev = open()?;
    let mut leds = Leds::new();
    leds.all_off();
    for i in lo..hi {
        leds.raw_mut()[i] = value;
    }
    leds.flush(&mut dev)?;
    println!("slots {lo}..{hi} set to 0x{value:02x}; press enter to clear");
    prompt("")?;
    leds.all_off();
    leds.flush(&mut dev)?;
    Ok(())
}

fn parse_u8(s: &str) -> Result<u8> {
    let t = s.trim();
    Ok(match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16)?,
        None => t.parse()?,
    })
}

// ---------------------------------------------------------------------------

/// Find which LED slots drive the 16 pads, and record the base in the config.
///
/// The two banks are 62 and 41 slots, and 41 is exactly 25 touch-strip LEDs
/// plus 16 pads -- so the pads start at either end of the second bank. Those
/// two candidates are tried first; anything else falls back to a binary search
/// over the whole array, which needs about seven answers.
fn find_pads(args: &[String]) -> Result<()> {
    let path = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(Config::default_path);

    let mut dev = open()?;
    let mut leds = Leds::new();

    println!(
        "\
Looking for the LED slots that drive the 4x4 pad grid.

Answer y or n to each question. Only the pads matter -- ignore any other
button that lights up along the way. Make sure mk3d is not running, or it
will keep overwriting what this tool sets.
"
    );

    let light = |leds: &mut Leds, dev: &mut HidDev, lo: usize, hi: usize| -> Result<()> {
        leds.all_off();
        for i in lo..hi.min(LED_COUNT) {
            leds.raw_mut()[i] = 0x47;
        }
        leds.flush(dev)?;
        Ok(())
    };

    let ask = |q: &str| -> Result<bool> {
        loop {
            match prompt(q)?.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => eprintln!("  please answer y or n"),
            }
        }
    };

    let mut base = None;
    for cand in [leds::BANK0_LEN, LED_COUNT - 16] {
        light(&mut leds, &mut dev, cand, cand + 16)?;
        if ask(&format!("slots {cand}..{}: are all 16 pads lit? (y/n) ", cand + 16))? {
            base = Some(cand);
            break;
        }
    }

    if base.is_none() {
        println!("\nFalling back to a search over every slot.");
        // Find the lowest slot that lights any pad, by halving the prefix.
        let (mut lo, mut hi) = (0usize, LED_COUNT);
        light(&mut leds, &mut dev, lo, hi)?;
        if !ask("all slots lit: is any pad lit at all? (y/n) ")? {
            bail!("no slot lights a pad -- is this really a Maschine MK3?");
        }
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            light(&mut leds, &mut dev, lo, mid)?;
            if ask(&format!("slots {lo}..{mid}: is any pad lit? (y/n) "))? {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        base = Some(lo);
    }

    let base = base.expect("set on every path above");

    // Confirm the block really is the 16 pads and nothing else.
    light(&mut leds, &mut dev, base, base + 16)?;
    if !ask(&format!(
        "\nslots {base}..{}: all 16 pads lit, and no pad left dark? (y/n) ",
        base + 16
    ))? {
        bail!("slots {base}..{} are not the pad block; run `mk3-learn leds {base}` to step through them one at a time", base + 16);
    }

    // Check the ordering, which is not guaranteed to ascend the way the
    // config's `notes` list does.
    leds.all_off();
    leds.raw_mut()[base] = 0x47;
    leds.flush(&mut dev)?;
    println!(
        "\nOne pad is lit. Counting the bottom-left pad as 1 and reading left to\n\
         right, bottom to top, which pad is it?"
    );
    let which = prompt("  pad number (1-16)> ")?;
    let which: usize = which.parse().unwrap_or(0);
    match which {
        1 => println!("  ascending from the bottom-left, as the config assumes"),
        13 => println!(
            "  NOTE: slot {base} is the top-left pad, so the grid runs top-down.\n\
             Reorder pads.notes to match, or renumber as you prefer."
        ),
        0 => println!("  skipped"),
        n => println!(
            "  NOTE: slot {base} is pad {n}, so pad 0 in the config is that pad.\n\
             Adjust pads.notes if the layout matters to you."
        ),
    }

    leds.all_off();
    leds.flush(&mut dev)?;

    let mut cfg = if path.exists() {
        Config::load(&path)?
    } else {
        maschine_mk3::config_default::starter()
    };
    if cfg.pads.led_base == base {
        println!("\npads.led_base is already {base}; nothing to change.");
        return Ok(());
    }
    cfg.pads.led_base = base;
    cfg.validate()?;
    cfg.save(&path)?;
    println!("\nwrote pads.led_base = {base} to {}", path.display());
    Ok(())
}
