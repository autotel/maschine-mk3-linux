//! The configuration file.
//!
//! Everything the driver does to a control -- what MIDI it emits, how its LED
//! behaves, how velocity is shaped -- comes from here. The file is plain TOML,
//! hand-editable, and reloaded when it changes on disk.
//!
//! Hardware indices (`bit` for a button's HID bit, `led` for its LED slot) are
//! part of the config rather than compiled in, because the only reliable way
//! to learn them is to press the button and watch. `mk3-learn` writes them.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::hid::{KNOBS, PADS};

/// LED slot of pad 0.
///
/// The 103 LED slots are 62 button slots (output report `0x80`) followed by 41
/// more (report `0x81`): the 25 touch strip LEDs first, then the 16 pads.
/// Verified on hardware by lighting single slots -- see `docs/led-map.md`.
pub const PAD_LED_BASE: usize = 87;
/// LED slot of the touch strip's first LED.
pub const STRIP_LED_BASE: usize = 62;

/// Note numbers for [`PadCfg::notes`] laid out so the bottom-left pad is lowest.
///
/// The device numbers its pads in reading order -- HID pad 0 is the **top**
/// left pad, not the bottom left one, confirmed on hardware. Drum mapping
/// convention runs the other way, with the lowest note bottom left, so the
/// shipped default is transposed rather than a plain ascending run.
///
/// Indexed by HID pad index; the silkscreen number is 12 rows up from it.
pub const NOTES_BOTTOM_LEFT: [u8; PADS] = [
    48, 49, 50, 51, // HID 0-3   = top row,    silkscreen 13-16
    44, 45, 46, 47, // HID 4-7   = third row,  silkscreen 9-12
    40, 41, 42, 43, // HID 8-11  = second row, silkscreen 5-8
    36, 37, 38, 39, // HID 12-15 = bottom row, silkscreen 1-4
];
/// Number of LEDs in the touch strip.
pub const STRIP_LEDS: usize = 25;
use crate::midi::Msg;

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Process-wide settings.
    pub general: General,
    /// Display behaviour.
    pub display: DisplayCfg,
    /// Global LED behaviour.
    pub leds: LedCfg,
    /// The 16 pads.
    pub pads: PadCfg,
    /// The 8 knobs under the displays.
    pub knobs: KnobCfg,
    /// The 4-D push encoder.
    pub encoder: EncoderCfg,
    /// The touch strip.
    pub touchstrip: StripCfg,
    /// The rear pedal jack.
    pub pedal: PedalCfg,
    /// Named buttons, keyed by the name shown in logs and the GUI.
    pub button: BTreeMap<String, ButtonCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            display: DisplayCfg::default(),
            leds: LedCfg::default(),
            pads: PadCfg::default(),
            knobs: KnobCfg::default(),
            encoder: EncoderCfg::default(),
            touchstrip: StripCfg::default(),
            pedal: PedalCfg::default(),
            button: BTreeMap::new(),
        }
    }
}

/// Process-wide settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// ALSA sequencer client name.
    pub client_name: String,
    /// Name of the port a DAW reads from.
    pub out_port: String,
    /// Name of the port a DAW writes to for feedback.
    pub in_port: String,
    /// `SCHED_FIFO` priority for the input thread; 0 disables real-time.
    pub realtime_priority: i32,
    /// Whether to `mlockall`, avoiding page faults in the input path.
    pub lock_memory: bool,
    /// TCP port for the built-in configuration GUI; 0 disables it.
    pub gui_port: u16,
    /// Address the GUI listens on. Loopback by default.
    pub gui_bind: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            client_name: "Maschine MK3".into(),
            out_port: "Controller Out".into(),
            in_port: "Controller In".into(),
            realtime_priority: 80,
            lock_memory: true,
            gui_port: 8730,
            gui_bind: "127.0.0.1".into(),
        }
    }
}

/// Display behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayCfg {
    /// Whether to drive the screens at all.
    pub enabled: bool,
    /// Backlight, 0..=100.
    pub brightness: u8,
    /// Contrast, 0..=100.
    pub contrast: u8,
    /// Redraw ceiling. The screens are cosmetic; capping them keeps the bulk
    /// endpoint from competing with input for USB bandwidth.
    pub fps: u32,
    /// Text shown on the left screen's title bar.
    pub title: String,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            brightness: 80,
            contrast: 50,
            fps: 30,
            title: "MASCHINE MK3".into(),
        }
    }
}

/// Global LED behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LedCfg {
    /// Whether to drive LEDs at all.
    pub enabled: bool,
    /// Brightness of an unlit button, 0..=127.
    pub button_idle: u8,
    /// Brightness of a held button, 0..=127.
    pub button_active: u8,
    /// Ceiling on LED report rate.
    pub fps: u32,
}

impl Default for LedCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            button_idle: 10,
            button_active: 127,
            fps: 60,
        }
    }
}

/// How a pad hit becomes velocity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Curve {
    /// Straight scaling of the 12-bit value.
    Linear,
    /// Expands the quiet end; easier to play softly.
    Soft,
    /// Compresses the quiet end; needs a firmer hit.
    Hard,
    /// Ignore how hard the pad was struck.
    Fixed,
}

/// Aftertouch handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aftertouch {
    /// Discard pressure data.
    Off,
    /// One pressure stream per pad (polyphonic key pressure).
    Poly,
    /// A single channel pressure stream, driven by the hardest-pressed pad.
    Channel,
}

/// The 16 pads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PadCfg {
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// Note number for each pad, in hardware order (pad 0 first).
    pub notes: Vec<u8>,
    /// Velocity shaping.
    pub curve: Curve,
    /// Velocity used when `curve = "fixed"`.
    pub fixed_velocity: u8,
    /// Raw hit value that maps to velocity 127.
    ///
    /// The pads report 12 bits but do not use the range: on a real MK3 a hard
    /// hit lands around 1900 and ordinary playing sits between 200 and 1400.
    /// Scaling against the full 4095 would make everything feel weak, so the
    /// default is set where the hardware actually tops out. Lower it for a
    /// lighter touch, raise it if you keep pinning at 127.
    pub velocity_max: u16,
    /// Raw pressure value that maps to aftertouch 127.
    ///
    /// Unlike strike velocity, pressure really does use the whole 12-bit
    /// range: leaning hard on a pad reaches 4094 on a real MK3.
    pub aftertouch_max: u16,
    /// Shaping applied to pressure, separate from the strike curve.
    ///
    /// Pressure is steeply non-linear: on the measured ramp it loiters between
    /// 30 and 150 through most of the comfortable range and only then climbs
    /// to 4094 in the last part of the travel. Linear scaling therefore leaves
    /// aftertouch near zero for almost the whole gesture, so the default lifts
    /// the quiet end.
    pub aftertouch_curve: Curve,
    /// Ignore hits below this 12-bit value; suppresses crosstalk from
    /// neighbouring pads.
    pub threshold: u16,
    /// Pressure handling.
    pub aftertouch: Aftertouch,
    /// Pressure below this 12-bit value is not transmitted.
    pub aftertouch_floor: u16,
    /// Emit a note when a finger rests on a pad without striking it.
    pub press_sends_note: bool,
    /// Palette index of an idle pad.
    pub idle_colour: u8,
    /// Palette index of a struck pad.
    pub active_colour: u8,
    /// Brightness step of an idle pad, 0..=3.
    pub idle_level: u8,
    /// LED slot of pad 0; the remaining pads follow consecutively.
    pub led_base: usize,
}

impl Default for PadCfg {
    fn default() -> Self {
        Self {
            channel: 10,
            notes: NOTES_BOTTOM_LEFT.to_vec(),
            curve: Curve::Linear,
            fixed_velocity: 100,
            velocity_max: 2000,
            aftertouch_max: 4095,
            aftertouch_curve: Curve::Soft,
            threshold: 0,
            aftertouch: Aftertouch::Poly,
            aftertouch_floor: 64,
            press_sends_note: false,
            idle_colour: 11,
            active_colour: 17,
            idle_level: 1,
            led_base: PAD_LED_BASE,
        }
    }
}

impl PadCfg {
    /// Map a raw pressure reading to a 7-bit aftertouch value.
    ///
    /// Returns 0 below the floor, so a resting hand stops transmitting rather
    /// than idling at some small non-zero pressure.
    pub fn pressure(&self, raw: u16) -> u8 {
        if raw <= self.aftertouch_floor {
            return 0;
        }
        let span = self.aftertouch_max.saturating_sub(self.aftertouch_floor).max(1);
        let x = ((raw - self.aftertouch_floor).min(span) as f32) / span as f32;
        let y = match self.aftertouch_curve {
            Curve::Linear | Curve::Fixed => x,
            Curve::Soft => x.sqrt(),
            Curve::Hard => x * x,
        };
        ((y * 127.0).round() as i32).clamp(0, 127) as u8
    }

    /// Map a raw pad reading to a 7-bit velocity, honouring the curve.
    ///
    /// Never returns 0 for a real hit: a zero-velocity note on is a note off.
    pub fn velocity(&self, raw: u16) -> u8 {
        if self.curve == Curve::Fixed {
            return self.fixed_velocity.clamp(1, 127);
        }
        let full = self.velocity_max.max(1);
        let x = (raw.min(full) as f32) / full as f32;
        let y = match self.curve {
            Curve::Linear | Curve::Fixed => x,
            // Square root lifts quiet hits, square pushes them down.
            Curve::Soft => x.sqrt(),
            Curve::Hard => x * x,
        };
        ((y * 127.0).round() as i32).clamp(1, 127) as u8
    }
}

/// Knob transmission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KnobMode {
    /// Send the knob's position.
    Absolute,
    /// Send the change since the last report, in a relative CC encoding.
    Relative,
}

/// How a host should read relative CCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelFormat {
    /// 1 = +1, 127 = -1 (two's complement). Ableton "Relative (Signed Bit 2)".
    Twos,
    /// 65 = +1, 63 = -1 (offset by 64). Ableton "Relative (Bin Offset)".
    BinOffset,
    /// 1 = +1, 65 = -1 (sign in bit 6). Ableton "Relative (Signed Bit)".
    SignBit,
}

impl RelFormat {
    /// Encode a signed step as a 7-bit CC value.
    pub fn encode(self, delta: i32) -> u8 {
        let d = delta.clamp(-63, 63);
        match self {
            Self::Twos => (d & 0x7f) as u8,
            Self::BinOffset => (64 + d).clamp(0, 127) as u8,
            Self::SignBit => {
                if d >= 0 {
                    (d & 0x3f) as u8
                } else {
                    (0x40 | ((-d) & 0x3f)) as u8
                }
            }
        }
    }
}

/// What happens when a knob's position and the host's value disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pickup {
    /// Send immediately; the host value jumps to the knob.
    Jump,
    /// Stay silent until the knob passes the last value the host reported.
    Pickup,
}

/// The 8 knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnobCfg {
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// CC number per knob.
    pub ccs: Vec<u8>,
    /// Absolute or relative.
    pub mode: KnobMode,
    /// Relative encoding, when `mode = "relative"`.
    pub relative_format: RelFormat,
    /// Send 14-bit values as `cc` plus `cc + 32` (MIDI LSB convention).
    pub high_resolution: bool,
    /// Soft-takeover behaviour in absolute mode.
    pub pickup: Pickup,
    /// Ignore movements smaller than this many raw units; the knobs are noisy
    /// at rest and this stops a stream of redundant CCs.
    pub deadband: u16,
}

impl Default for KnobCfg {
    fn default() -> Self {
        Self {
            channel: 1,
            ccs: (16..16 + KNOBS as u8).collect(),
            mode: KnobMode::Absolute,
            relative_format: RelFormat::Twos,
            high_resolution: false,
            pickup: Pickup::Jump,
            deadband: 2,
        }
    }
}

/// The 4-D push encoder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EncoderCfg {
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// CC used for rotation.
    pub cc: u8,
    /// Absolute (accumulates internally) or relative.
    pub mode: KnobMode,
    /// Relative encoding, when `mode = "relative"`.
    pub relative_format: RelFormat,
    /// Multiplier applied to each detent.
    pub step: i32,
}

impl Default for EncoderCfg {
    fn default() -> Self {
        Self {
            channel: 1,
            cc: 24,
            mode: KnobMode::Relative,
            relative_format: RelFormat::Twos,
            step: 1,
        }
    }
}

/// The touch strip: one continuous input plus a 25-LED meter beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StripCfg {
    /// Whether to transmit at all.
    pub enabled: bool,
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// CC number.
    pub cc: u8,
    /// Which analog field the strip reports on.
    ///
    /// Field 1 on the units tested. `mk3-learn watch` prints all seven while
    /// you slide a finger; if a different one moves, change this rather than
    /// patching the driver. Field 0 is a free-running counter and must not be
    /// used -- it would emit a CC on every report forever.
    pub source: usize,
    /// Value the strip reads at full deflection.
    pub max: u16,
    /// Light the strip's LEDs to follow the finger.
    pub leds: bool,
    /// LED slot of the strip's first LED.
    pub led_base: usize,
    /// How many LEDs the strip has.
    pub led_count: usize,
    /// Fill the meter from the high slot downwards.
    ///
    /// Slot order ascends along the strip, but which physical end that starts
    /// from relative to the finger's travel has not been confirmed. If the
    /// meter fills from the wrong end, flip this.
    pub led_reversed: bool,
    /// Raw LED byte written to a lit segment.
    ///
    /// The strip does not decode colour the way the pads do -- writing the
    /// pads' green renders as violet here -- so this is a raw value rather
    /// than a palette index. Try values between 1 and 127 to taste.
    pub led_value: u8,
}

impl Default for StripCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: 1,
            cc: 1,
            source: crate::hid::STRIP_ANALOG,
            max: crate::hid::STRIP_MAX,
            leds: true,
            led_base: STRIP_LED_BASE,
            led_count: STRIP_LEDS,
            led_reversed: false,
            led_value: 0x1f,
        }
    }
}

/// The rear pedal jack, which carries up to two switches or expression pedals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PedalCfg {
    /// Whether to transmit at all.
    pub enabled: bool,
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// CC number per pedal.
    pub ccs: Vec<u8>,
    /// Treat the input as a switch (0 or 127) rather than continuous.
    pub switch: bool,
    /// Switch threshold, as a 12-bit value.
    pub switch_threshold: u16,
}

impl Default for PedalCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: 1,
            ccs: vec![64, 67],
            switch: true,
            switch_threshold: 2048,
        }
    }
}

/// How a button's MIDI output behaves across presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonMode {
    /// On while held, off on release.
    Momentary,
    /// Each press flips between on and off.
    Toggle,
    /// Send only on press, nothing on release.
    Trigger,
}

/// Where a button's LED takes its state from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LedMode {
    /// Mirror the button's own state.
    Follow,
    /// Mirror whatever the host sends back on the button's own MIDI address.
    Midi,
    /// Always lit at the idle brightness.
    Always,
    /// Never lit.
    Off,
}

/// One named button.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ButtonCfg {
    /// HID button-bit index, 0..80.
    pub bit: usize,
    /// LED slot, or `-1` when the button has no LED.
    pub led: i32,
    /// MIDI to emit, in the compact form documented on [`Action`].
    pub midi: String,
    /// Press/release behaviour.
    pub mode: ButtonMode,
    /// LED source.
    pub led_mode: LedMode,
}

impl Default for ButtonCfg {
    fn default() -> Self {
        Self {
            bit: 0,
            led: -1,
            midi: "none".into(),
            mode: ButtonMode::Momentary,
            led_mode: LedMode::Follow,
        }
    }
}

/// A parsed `midi = "..."` string.
///
/// Accepted forms:
///
/// | string              | meaning                                  |
/// |---------------------|------------------------------------------|
/// | `none`              | emit nothing                             |
/// | `note <ch> <n>`     | note on/off, velocity 127                |
/// | `cc <ch> <n>`       | control change, 127 on / 0 off           |
/// | `cc <ch> <n> <v>`   | control change with an explicit on value |
/// | `pc <ch> <n>`       | program change on press                  |
/// | `start`/`stop`/`continue` | transport message on press         |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Emit nothing.
    None,
    /// Note on/off.
    Note { ch: u8, note: u8, vel: u8 },
    /// Control change with an explicit "on" value.
    Cc { ch: u8, cc: u8, on: u8 },
    /// Program change, press only.
    Program { ch: u8, num: u8 },
    /// Transport message, press only.
    Transport(Transport),
}

/// The transport messages a button can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// MIDI start.
    Start,
    /// MIDI stop.
    Stop,
    /// MIDI continue.
    Continue,
}

impl Action {
    /// Parse the compact form.
    pub fn parse(s: &str) -> Result<Self> {
        let mut it = s.split_whitespace();
        let Some(kind) = it.next() else {
            return Ok(Action::None);
        };
        let num = |o: Option<&str>, what: &str| -> Result<u8> {
            o.ok_or_else(|| anyhow!("`{s}`: missing {what}"))?
                .parse::<u8>()
                .with_context(|| format!("`{s}`: bad {what}"))
        };
        let chan = |o: Option<&str>| -> Result<u8> {
            let c = num(o, "channel")?;
            if !(1..=16).contains(&c) {
                bail!("`{s}`: channel {c} outside 1..16");
            }
            Ok(c - 1)
        };
        let data = |o: Option<&str>, what: &str| -> Result<u8> {
            let v = num(o, what)?;
            if v > 127 {
                bail!("`{s}`: {what} {v} outside 0..127");
            }
            Ok(v)
        };
        Ok(match kind {
            "none" | "" => Action::None,
            "note" => Action::Note {
                ch: chan(it.next())?,
                note: data(it.next(), "note")?,
                vel: 127,
            },
            "cc" => {
                let ch = chan(it.next())?;
                let cc = data(it.next(), "cc")?;
                let on = match it.next() {
                    Some(v) => data(Some(v), "value")?,
                    None => 127,
                };
                Action::Cc { ch, cc, on }
            }
            "pc" => Action::Program {
                ch: chan(it.next())?,
                num: data(it.next(), "program")?,
            },
            "start" => Action::Transport(Transport::Start),
            "stop" => Action::Transport(Transport::Stop),
            "continue" => Action::Transport(Transport::Continue),
            other => bail!("`{s}`: unknown action `{other}`"),
        })
    }

    /// The message for a state change, or `None` when nothing should be sent.
    pub fn message(self, on: bool) -> Option<Msg> {
        match self {
            Action::None => None,
            Action::Note { ch, note, vel } => Some(if on {
                Msg::NoteOn { ch, note, vel }
            } else {
                Msg::NoteOff { ch, note, vel: 0 }
            }),
            Action::Cc { ch, cc, on: v } => Some(Msg::Cc {
                ch,
                cc,
                val: if on { v } else { 0 },
            }),
            Action::Program { ch, num } => on.then_some(Msg::Program { ch, num }),
            Action::Transport(t) => on.then_some(match t {
                Transport::Start => Msg::Start,
                Transport::Stop => Msg::Stop,
                Transport::Continue => Msg::Continue,
            }),
        }
    }
}

impl Config {
    /// Default location: `$XDG_CONFIG_HOME/maschine-mk3/config.toml`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("maschine-mk3").join("config.toml")
    }

    /// Read and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(cfg)
    }

    /// Serialise back to TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising config")
    }

    /// Write to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, self.to_toml()?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Rewrite only the `[button.*]` tables of an existing config file.
    ///
    /// [`Config::save`] round-trips through the serialiser, which throws away
    /// every comment in the file -- and the shipped config is mostly comments
    /// explaining what the fields do. Discovery tools write often and would
    /// strip the file bare on their first run, so they use this instead: the
    /// text before the button tables is preserved byte for byte, and only the
    /// button tables are regenerated.
    ///
    /// Comments written *inside* a `[button.x]` table are not preserved; there
    /// is nowhere to put them once the table is regenerated.
    pub fn write_buttons_preserving(path: &Path, buttons: &BTreeMap<String, ButtonCfg>) -> Result<()> {
        let original = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::config_default::STARTER_TOML.to_string()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };

        let mut kept = String::with_capacity(original.len());
        let mut in_button_table = false;
        for line in original.lines() {
            let t = line.trim_start();
            // Only an unindented, uncommented table header can close or open a
            // section; a `#` line is prose and stays put.
            if t.starts_with('[') {
                in_button_table = t.starts_with("[button.") || t.starts_with("[button]");
            }
            if !in_button_table {
                kept.push_str(line);
                kept.push('\n');
            }
        }
        while kept.ends_with("\n\n") {
            kept.pop();
        }

        let mut out = kept;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        // `toml::to_string` only serialises tables, so each scalar goes
        // through `Value` to get correct quoting and escaping.
        for (name, b) in buttons {
            out.push_str(&format!(
                "\n[button.{name}]\nbit = {}\nled = {}\nmidi = {}\nmode = {}\nled_mode = {}\n",
                b.bit,
                b.led,
                scalar(&b.midi),
                scalar(&b.mode),
                scalar(&b.led_mode),
            ));
        }

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Write via a temporary file so an interrupted run cannot leave a
        // half-written config behind.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &out).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Reject anything the engine would have to guess about at runtime.
    pub fn validate(&self) -> Result<()> {
        let chan = |name: &str, c: u8| -> Result<()> {
            if !(1..=16).contains(&c) {
                bail!("{name}: channel {c} outside 1..16");
            }
            Ok(())
        };
        chan("pads", self.pads.channel)?;
        chan("knobs", self.knobs.channel)?;
        chan("encoder", self.encoder.channel)?;
        chan("touchstrip", self.touchstrip.channel)?;
        chan("pedal", self.pedal.channel)?;

        if self.pads.notes.len() != PADS {
            bail!("pads.notes needs {PADS} entries, found {}", self.pads.notes.len());
        }
        if let Some(bad) = self.pads.notes.iter().find(|&&n| n > 127) {
            bail!("pads.notes contains {bad}, outside 0..127");
        }
        if self.knobs.ccs.len() != KNOBS {
            bail!("knobs.ccs needs {KNOBS} entries, found {}", self.knobs.ccs.len());
        }
        if self.knobs.high_resolution && self.knobs.ccs.iter().any(|&c| c >= 32) {
            bail!(
                "knobs.high_resolution needs CCs below 32 so the LSB can go to cc + 32; \
                 got {:?}",
                self.knobs.ccs
            );
        }
        if self.pedal.ccs.len() > 2 {
            bail!("pedal.ccs takes at most 2 entries");
        }
        if self.display.brightness > 100 || self.display.contrast > 100 {
            bail!("display brightness and contrast are 0..100");
        }
        if self.pads.idle_level > 3 {
            bail!("pads.idle_level is 0..3");
        }
        if self.pads.velocity_max == 0 {
            bail!("pads.velocity_max must be above 0");
        }
        if self.pads.aftertouch_max <= self.pads.aftertouch_floor {
            bail!(
                "pads.aftertouch_max ({}) must exceed pads.aftertouch_floor ({})",
                self.pads.aftertouch_max,
                self.pads.aftertouch_floor
            );
        }
        if self.touchstrip.max == 0 {
            bail!("touchstrip.max must be above 0");
        }
        if self.touchstrip.enabled && self.touchstrip.source == 0 {
            bail!(
                "touchstrip.source 0 is the device's free-running counter, not a control; \
                 field 1 is the strip"
            );
        }
        if self.pads.led_base + PADS > crate::leds::LED_COUNT {
            bail!(
                "pads.led_base {} puts pad {} past the last LED slot ({})",
                self.pads.led_base,
                PADS - 1,
                crate::leds::LED_COUNT - 1
            );
        }
        if self.touchstrip.source >= crate::hid::ANALOGS {
            bail!(
                "touchstrip.source {} outside 0..{}",
                self.touchstrip.source,
                crate::hid::ANALOGS
            );
        }
        if self.touchstrip.leds
            && self.touchstrip.led_base + self.touchstrip.led_count > crate::leds::LED_COUNT
        {
            bail!(
                "touchstrip LEDs {}..{} run past the last slot ({})",
                self.touchstrip.led_base,
                self.touchstrip.led_base + self.touchstrip.led_count,
                crate::leds::LED_COUNT - 1
            );
        }
        if self.pads.led_base < self.touchstrip.led_base + self.touchstrip.led_count
            && self.touchstrip.led_base < self.pads.led_base + PADS
        {
            bail!("pads and touchstrip claim overlapping LED slots");
        }

        let mut seen: BTreeMap<usize, &str> = BTreeMap::new();
        for (name, b) in &self.button {
            if b.bit >= crate::hid::BUTTON_BITS {
                bail!("button.{name}: bit {} outside 0..80", b.bit);
            }
            if let Some(prev) = seen.insert(b.bit, name) {
                bail!("button.{name} and button.{prev} both claim bit {}", b.bit);
            }
            if b.led >= crate::leds::LED_COUNT as i32 {
                bail!(
                    "button.{name}: led {} outside 0..{}",
                    b.led,
                    crate::leds::LED_COUNT
                );
            }
            Action::parse(&b.midi).with_context(|| format!("button.{name}.midi"))?;
        }
        Ok(())
    }

    /// Pre-parse every button action so the input path never parses strings.
    pub fn compiled_buttons(&self) -> Vec<CompiledButton> {
        let mut v: Vec<CompiledButton> = self
            .button
            .iter()
            .map(|(name, b)| CompiledButton {
                name: name.clone(),
                bit: b.bit,
                led: b.led,
                action: Action::parse(&b.midi).unwrap_or(Action::None),
                mode: b.mode,
                led_mode: b.led_mode,
            })
            .collect();
        v.sort_by_key(|b| b.bit);
        v
    }
}

/// Render a serialisable scalar as a TOML literal, quoting and escaping it.
fn scalar<T: Serialize>(v: &T) -> String {
    match toml::Value::try_from(v) {
        Ok(val) => val.to_string(),
        // Every caller passes a plain string or a unit enum, so this is
        // unreachable in practice; falling back to an empty string keeps the
        // file parseable rather than truncating it.
        Err(_) => "\"\"".to_string(),
    }
}

/// A button with its action already parsed.
#[derive(Debug, Clone)]
pub struct CompiledButton {
    /// Config key.
    pub name: String,
    /// HID bit.
    pub bit: usize,
    /// LED slot, negative when absent.
    pub led: i32,
    /// Parsed action.
    pub action: Action,
    /// Press/release behaviour.
    pub mode: ButtonMode,
    /// LED source.
    pub led_mode: LedMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_parsing_rejects_out_of_range() {
        assert!(Action::parse("cc 0 10").is_err(), "channel 0 is not valid MIDI");
        assert!(Action::parse("cc 17 10").is_err());
        assert!(Action::parse("note 1 200").is_err());
        assert!(Action::parse("wat").is_err());
    }

    #[test]
    fn action_channels_are_zero_based_internally() {
        assert_eq!(
            Action::parse("cc 1 118").unwrap(),
            Action::Cc { ch: 0, cc: 118, on: 127 }
        );
        assert_eq!(
            Action::parse("cc 16 7 64").unwrap(),
            Action::Cc { ch: 15, cc: 7, on: 64 }
        );
    }

    #[test]
    fn transport_only_fires_on_press() {
        let a = Action::parse("start").unwrap();
        assert_eq!(a.message(true), Some(Msg::Start));
        assert_eq!(a.message(false), None);
    }

    #[test]
    fn relative_encodings_round_trip_sign() {
        assert_eq!(RelFormat::Twos.encode(1), 1);
        assert_eq!(RelFormat::Twos.encode(-1), 127);
        assert_eq!(RelFormat::BinOffset.encode(1), 65);
        assert_eq!(RelFormat::BinOffset.encode(-1), 63);
        assert_eq!(RelFormat::SignBit.encode(1), 1);
        assert_eq!(RelFormat::SignBit.encode(-1), 0x41);
    }

    #[test]
    fn velocity_never_returns_zero_for_a_hit() {
        let mut p = PadCfg::default();
        p.curve = Curve::Hard;
        assert_eq!(p.velocity(1), 1, "a faint hit must not become a note off");
        assert_eq!(p.velocity(4095), 127);
    }

    #[test]
    fn default_notes_put_the_lowest_note_bottom_left() {
        let c = Config::default();
        // HID 12 is the bottom-left pad; HID 0 is the top-left one.
        assert_eq!(c.pads.notes[12], 36, "bottom-left pad plays the lowest note");
        assert_eq!(c.pads.notes[0], 48, "top-left pad plays the highest row");
        assert_eq!(c.pads.notes[15], 39, "bottom row ascends left to right");
        let mut sorted = c.pads.notes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), PADS, "every pad gets a distinct note");
    }

    #[test]
    fn pressure_uses_the_whole_range_and_lifts_the_quiet_end() {
        let c = Config::default();
        // Measured ramp: a firm lean reaches 4094, but most of the comfortable
        // travel sits between 30 and 700.
        assert_eq!(c.pads.pressure(0), 0, "a released pad transmits nothing");
        assert_eq!(c.pads.pressure(40), 0, "and neither does one below the floor");
        assert_eq!(c.pads.pressure(4094), 127, "a firm lean reaches full scale");
        let mid = c.pads.pressure(700);
        assert!(
            (40..=70).contains(&mid),
            "the soft curve must put ordinary pressure mid-scale, got {mid}"
        );
        // Linear scaling is what this replaces; check it really would be worse.
        let mut linear = c.pads.clone();
        linear.aftertouch_curve = Curve::Linear;
        assert!(linear.pressure(700) < 25, "linear leaves it near silent");
    }

    #[test]
    fn pressure_is_monotonic() {
        let c = Config::default();
        let mut last = 0;
        for raw in (0..=4095).step_by(37) {
            let v = c.pads.pressure(raw);
            assert!(v >= last, "pressure dipped at raw {raw}");
            last = v;
        }
    }

    #[test]
    fn writing_buttons_keeps_the_rest_of_the_file_verbatim() {
        let dir = std::env::temp_dir().join(format!("mk3cfg{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let original = "\
# a comment that must survive
[pads]
channel = 10   # and this trailing one
notes = [48, 49, 50, 51, 44, 45, 46, 47, 40, 41, 42, 43, 36, 37, 38, 39]

# commented-out example, also prose
# [button.example]
# bit = 3

[button.old]
bit = 7
led = -1
midi = \"none\"
mode = \"momentary\"
led_mode = \"follow\"
";
        std::fs::write(&path, original).unwrap();

        let mut buttons = BTreeMap::new();
        buttons.insert(
            "play".to_string(),
            ButtonCfg { bit: 45, led: 21, midi: "cc 1 118".into(), ..Default::default() },
        );
        Config::write_buttons_preserving(&path, &buttons).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("# a comment that must survive"));
        assert!(out.contains("channel = 10   # and this trailing one"));
        assert!(out.contains("# commented-out example, also prose"));
        assert!(out.contains("[button.play]"));
        assert!(out.contains("bit = 45"));
        assert!(!out.contains("[button.old]"), "the retired button is gone");

        // And the result must still parse and validate.
        let back = Config::load(&path).unwrap();
        assert_eq!(back.button["play"].bit, 45);
        assert_eq!(back.button["play"].led, 21);
        assert_eq!(back.button["play"].midi, "cc 1 118");
        assert_eq!(back.pads.channel, 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_button_bits_are_rejected() {
        let mut c = Config::default();
        c.button.insert("a".into(), ButtonCfg { bit: 3, ..Default::default() });
        c.button.insert("b".into(), ButtonCfg { bit: 3, ..Default::default() });
        assert!(c.validate().is_err());
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let c = Config::default();
        let s = c.to_toml().unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        back.validate().unwrap();
        assert_eq!(back.pads.notes, c.pads.notes);
    }
}
