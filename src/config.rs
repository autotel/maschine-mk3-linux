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
/// Verified on hardware by lighting single slots -- see `docs/hardware-map.md`.
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
    /// Where this set of settings came from, and what it is for.
    ///
    /// Optional: a config written by hand needs no header. It is filled in
    /// when the file is saved as a preset, so a file passed to someone else
    /// says what it is.
    #[serde(default)]
    pub preset: Option<PresetInfo>,
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
    /// What each named control should send, keyed by its name in the device
    /// profile.
    pub buttons: BTreeMap<String, Binding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preset: None,
            general: General::default(),
            display: DisplayCfg::default(),
            leds: LedCfg::default(),
            pads: PadCfg::default(),
            knobs: KnobCfg::default(),
            encoder: EncoderCfg::default(),
            touchstrip: StripCfg::default(),
            pedal: PedalCfg::default(),
            buttons: BTreeMap::new(),
        }
    }
}

/// What a preset file calls itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetInfo {
    /// Short name, matching the file name.
    pub name: String,
    /// One line saying what it is for. Shown in the chooser.
    pub description: String,
    /// Who made it, for a preset that came from someone else.
    pub author: String,
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
    /// TCP port for the browser-based configuration page; 0 disables it.
    ///
    /// Off by default: `mk3-gui` is a native window talking over a Unix
    /// socket, which needs no port and cannot be reached from the network.
    /// This remains for configuring a machine you are only logged into
    /// remotely.
    pub gui_port: u16,
    /// Address the GUI listens on. Loopback by default.
    pub gui_bind: String,
    /// Sequencer destinations to connect our output to at startup.
    ///
    /// Matching is case-insensitive on a substring of `client:port`. Some
    /// hosts list a MIDI input without ever subscribing to it, which is
    /// indistinguishable from a driver that is not sending; naming the host
    /// here makes the connection from this side instead. Empty means connect
    /// to nothing and wait to be subscribed.
    pub connect_to: Vec<String>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            client_name: "Maschine MK3".into(),
            out_port: "Controller Out".into(),
            in_port: "Controller In".into(),
            realtime_priority: 80,
            lock_memory: true,
            gui_port: 0,
            gui_bind: "127.0.0.1".into(),
            connect_to: Vec::new(),
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
    /// Send the knob's reported position directly.
    ///
    /// Only meaningful for a control that has end stops. The MK3's knobs do
    /// not: they are endless encoders whose position rolls over from 999 to 0,
    /// so this mode makes them jump. Use [`KnobMode::Accumulate`] instead.
    Absolute,
    /// Send the change since the last report, in a relative CC encoding.
    ///
    /// The host does the integrating, which needs it to be told the encoding.
    Relative,
    /// Integrate movement here and send the running total, clamped at the ends.
    ///
    /// This is what makes an endless encoder behave like a knob with end
    /// stops: turning up eventually reaches 127 and stays there rather than
    /// wrapping round to 0. It needs no host support.
    Accumulate,
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
    /// Raw units of travel that span the full output range, in accumulate mode.
    ///
    /// The knobs report 0..999 over one turn, so 1000 makes a single turn
    /// sweep the whole CC range. Halve it to make them twice as fast.
    pub travel: u16,
    /// Where each knob sits when the driver starts, in accumulate mode.
    ///
    /// An endless encoder has no position to read back, so the starting value
    /// has to be assumed. Centring is the least surprising choice.
    pub initial: u8,
}

impl Default for KnobCfg {
    fn default() -> Self {
        Self {
            channel: 1,
            ccs: (16..16 + KNOBS as u8).collect(),
            mode: KnobMode::Accumulate,
            relative_format: RelFormat::Twos,
            high_resolution: false,
            pickup: Pickup::Jump,
            deadband: 2,
            travel: 1000,
            initial: 64,
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
    ///
    /// At 1 a full sweep of 0..127 takes 128 clicks, which is a lot of
    /// twisting; 2 halves it.
    pub step: i32,
    /// Where the encoder sits when the driver starts.
    pub initial: u8,
}

impl Default for EncoderCfg {
    fn default() -> Self {
        Self {
            channel: 1,
            cc: 24,
            mode: KnobMode::Absolute,
            relative_format: RelFormat::Twos,
            step: 2,
            initial: 64,
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
    /// Light the strip's LEDs to follow the finger.
    pub leds: bool,
    /// Fill the meter from the far end.
    ///
    /// Which physical end the profile's slot order starts from, relative to
    /// the finger's travel, is a property of the hardware; this flips it if it
    /// comes out backwards on your unit.
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
            leds: true,
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

/// What a named control should do.
///
/// Written either as a bare string, which is the common case:
///
/// ```toml
/// [buttons]
/// play = "cc 1 118"
/// ```
///
/// or as a table when something other than the defaults is wanted:
///
/// ```toml
/// [buttons]
/// mute = { send = "cc 16 37", mode = "toggle" }
/// ```
///
/// Where the control's bit and LED live is not written here at all -- that is
/// in the device profile, which is a description of the hardware rather than
/// of anyone's preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Binding {
    /// Just what to send; press behaviour and LED take their defaults.
    Send(String),
    /// The full form.
    Full(ButtonCfg),
}

impl Binding {
    /// Normalise either form into the full one.
    pub fn resolve(&self) -> ButtonCfg {
        match self {
            Binding::Send(s) => ButtonCfg {
                send: s.clone(),
                ..Default::default()
            },
            Binding::Full(c) => c.clone(),
        }
    }
}

/// The long form of a control binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ButtonCfg {
    /// MIDI to emit, in the compact form documented on [`Action`].
    pub send: String,
    /// Press/release behaviour.
    pub mode: ButtonMode,
    /// Where the LED takes its state from.
    pub led: LedMode,
}

impl Default for ButtonCfg {
    fn default() -> Self {
        Self {
            send: "none".into(),
            mode: ButtonMode::Momentary,
            led: LedMode::Follow,
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

    /// Write this config to `path`, preserving the file's comments.
    ///
    /// [`Config::save`] round-trips through the serialiser, which throws away
    /// every comment -- and the shipped file is mostly comments explaining
    /// what the fields do. Anything that writes a config a human is expected
    /// to keep reading must come through here instead: the existing document
    /// is edited in place, so only the values change.
    ///
    /// A comment written *inside* a `[button.x]` table that no longer exists
    /// goes with the table. Everything else survives.
    pub fn save_preserving(&self, path: &Path) -> Result<()> {
        let original = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::config_default::STARTER_TOML.to_string()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut doc: toml_edit::DocumentMut = original
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;

        let fresh: toml_edit::DocumentMut = toml_edit::ser::to_document(self)
            .context("serialising config")?;

        for (key, item) in fresh.iter() {
            match doc.get_mut(key) {
                // The table already exists, so update its leaves and leave the
                // prose around them alone.
                Some(existing) => merge_preserving(existing, item),
                None => {
                    doc[key] = expand_inline_tables(item);
                }
            }
        }
        // Drop bindings the config no longer has.
        if let Some(buttons) = doc.get_mut("buttons").and_then(|b| b.as_table_mut()) {
            let keep: Vec<String> = self.buttons.keys().cloned().collect();
            buttons.retain(|k, _| keep.iter().any(|n| n == k));
        }

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Write via a temporary file so an interrupted run cannot leave a
        // half-written config behind.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Reject anything the engine would have to guess about at runtime.
    ///
    /// Only settings are checked here. Whether a control exists at all is a
    /// question about the hardware, so it is checked against the profile by
    /// [`Config::validate_against`].
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
        if self.knobs.travel == 0 {
            bail!("knobs.travel must be above 0");
        }
        if self.knobs.initial > 127 {
            bail!("knobs.initial is 0..127");
        }
        if self.encoder.initial > 127 {
            bail!("encoder.initial is 0..127");
        }
        if self.encoder.step == 0 {
            bail!("encoder.step must not be 0");
        }

        for (name, binding) in &self.buttons {
            let b = binding.resolve();
            Action::parse(&b.send).with_context(|| format!("buttons.{name}"))?;
        }
        Ok(())
    }

    /// Check the config against the hardware it will run on.
    ///
    /// A name here that the profile does not have is almost always a typo, and
    /// silently ignoring it means a control that simply never works with no
    /// clue as to why. The message lists near matches.
    pub fn validate_against(&self, profile: &crate::profile::Profile) -> Result<()> {
        self.validate()?;
        for name in self.buttons.keys() {
            if profile.get(name).is_some() {
                continue;
            }
            // A plain substring test matches far too eagerly -- "plya" would
            // suggest "a" -- so require a shared run of at least three
            // characters, or a single-character slip.
            let near: Vec<&String> = profile
                .control
                .keys()
                .filter(|k| {
                    (k.len() >= 3 && (k.contains(name.as_str()) || name.contains(k.as_str())))
                        || edit_distance_at_most(k, name, 2)
                })
                .take(4)
                .collect();
            if near.is_empty() {
                bail!("buttons.{name}: no control called that on this device");
            }
            bail!(
                "buttons.{name}: no control called that; did you mean {}?",
                near.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            );
        }
        Ok(())
    }

    /// Join the config's bindings with the profile's hardware facts.
    ///
    /// Pre-parsing here keeps the input path free of string handling, and
    /// joining here is what lets the config talk about `play` while the engine
    /// works in bit indices.
    pub fn compiled_buttons(&self, profile: &crate::profile::Profile) -> Vec<CompiledButton> {
        let mut v: Vec<CompiledButton> = Vec::new();
        for (name, binding) in &self.buttons {
            let Some(control) = profile.get(name) else {
                continue;
            };
            let Some(bit) = control.bit else { continue };
            let b = binding.resolve();
            v.push(CompiledButton {
                name: name.clone(),
                bit,
                led: control.led.map(|l| l as i32).unwrap_or(-1),
                led_colour: control.led_colour.map(|c| c as i32).unwrap_or(-1),
                action: Action::parse(&b.send).unwrap_or(Action::None),
                mode: b.mode,
                led_mode: b.led,
            });
        }
        v.sort_by_key(|b| b.bit);
        v
    }
}


/// Whether `a` and `b` are within `max` single-character edits of each other.
///
/// Only used to suggest a name after a typo, so it bails out early rather than
/// computing an exact distance for strings that are obviously unrelated.
fn edit_distance_at_most(a: &str, b: &str, max: usize) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        if cur.iter().min().copied().unwrap_or(usize::MAX) > max {
            return false;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()] <= max
}

/// Rewrite inline tables as full `[section]` tables, recursively.
///
/// `toml_edit`'s serializer emits an inline table for anything it builds from
/// scratch, so a section the file did not already have arrives as one very
/// long line. That is valid TOML and unreadable, and an inline table cannot
/// carry a comment of its own -- which matters here, because these files are
/// meant to be edited by hand.
///
/// Arrays are left alone: `ccs = [16, 17, ...]` belongs on one line.
pub fn expand_inline_tables(item: &toml_edit::Item) -> toml_edit::Item {
    let table = match item {
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => t.clone().into_table(),
        toml_edit::Item::Table(t) => t.clone(),
        other => return other.clone(),
    };
    let mut out = toml_edit::Table::new();
    *out.decor_mut() = table.decor().clone();
    for (k, v) in table.iter() {
        out.insert(k, expand_inline_tables(v));
    }
    toml_edit::Item::Table(out)
}

/// Copy the values of `fresh` into `existing`, leaving its formatting alone.
///
/// Recursing rather than assigning wholesale is the point: replacing a table
/// would replace its comments with none.
pub fn merge_preserving(existing: &mut toml_edit::Item, fresh: &toml_edit::Item) {
    match (existing.as_table_like_mut(), fresh.as_table_like()) {
        (Some(dst), Some(src)) => {
            for (k, v) in src.iter() {
                match dst.get_mut(k) {
                    Some(slot) if v.as_table_like().is_some() => merge_preserving(slot, v),
                    Some(slot) => {
                        // Carry the old value's decor across. That is where
                        // both the comment above a setting and the one
                        // trailing it on the same line are stored; assigning
                        // the fresh value wholesale would drop them.
                        match (slot.as_value(), v.as_value()) {
                            (Some(old), Some(new)) => {
                                let mut replacement = new.clone();
                                *replacement.decor_mut() = old.decor().clone();
                                *slot = toml_edit::Item::Value(replacement);
                            }
                            _ => *slot = v.clone(),
                        }
                    }
                    None => {
                        dst.insert(k, expand_inline_tables(v));
                    }
                }
            }
        }
        _ => *existing = fresh.clone(),
    }
}

/// A button with its action already parsed.
#[derive(Debug, Clone)]
pub struct CompiledButton {
    /// Control name, as the profile and the config both write it.
    pub name: String,
    /// HID bit, from the profile.
    pub bit: usize,
    /// LED slot from the profile, negative when the control has no light.
    pub led: i32,
    /// Palette index for a colour LED, negative for monochrome.
    pub led_colour: i32,
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
    fn a_name_the_device_does_not_have_is_rejected() {
        use crate::profile::Profile;
        let mut c = Config::default();
        c.buttons.insert("plya".into(), Binding::Send("cc 1 1".into()));
        let err = c
            .validate_against(&Profile::builtin())
            .unwrap_err()
            .to_string();
        assert!(err.contains("plya"), "{err}");
        assert!(err.contains("play"), "a typo should suggest the real name: {err}");
    }

    #[test]
    fn shorthand_and_full_forms_agree() {
        let short = Binding::Send("cc 1 118".into()).resolve();
        assert_eq!(short.send, "cc 1 118");
        assert_eq!(short.mode, ButtonMode::Momentary);
        assert_eq!(short.led, LedMode::Follow);
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
