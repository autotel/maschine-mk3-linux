//! Device profiles: what the hardware *is*, as data.
//!
//! Which bit in the HID report is Play, which LED slot lights it, where it
//! sits on the panel -- none of that is a preference, and none of it belongs
//! in a user's config file. It lives in a profile so that:
//!
//! * a user's settings survive a correction to the hardware map, and vice
//!   versa;
//! * the config can name controls the way the panel does (`play`, `mute`)
//!   instead of by bit index;
//! * supporting another Native Instruments controller is a matter of writing a
//!   new profile rather than editing the driver. The report layouts differ
//!   between models in offsets and counts, not in kind.
//!
//! The Maschine MK3 profile is compiled in, so the driver works with nothing
//! installed. A file of the same name in the config directory overrides it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The profile shipped with the driver.
pub const BUILTIN_MK3: &str = include_str!("../devices/maschine-mk3.toml");

/// A complete description of one controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Identity.
    pub device: DeviceInfo,
    /// Report ids and LED bank sizes.
    pub report: Reports,
    /// Field offsets and counts within those reports.
    pub layout: Layout,
    /// Every control, keyed by the name the config uses.
    pub control: BTreeMap<String, Control>,
}

/// Which device a profile is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInfo {
    /// Human-readable name.
    pub name: String,
    /// USB vendor id.
    pub vendor: u16,
    /// USB product id.
    pub product: u16,
}

/// HID report ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reports {
    /// Input report carrying buttons, knobs, encoder and analog fields.
    pub controls: u8,
    /// Input report carrying pad events.
    pub pads: u8,
    /// Output reports carrying LED state, as `[report id, byte length]`.
    ///
    /// The driver addresses them as one flat array, in this order.
    pub led_banks: Vec<(u8, usize)>,
}

impl Reports {
    /// Total addressable LED slots.
    pub fn led_count(&self) -> usize {
        self.led_banks.iter().map(|(_, n)| n).sum()
    }
}

/// Where things sit inside the reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Byte offset of the button bitfield.
    pub buttons_offset: usize,
    /// How many button bits there are.
    pub button_bits: usize,
    /// Byte offset of the encoder's nibble counter.
    pub encoder_offset: usize,
    /// Byte offset of the first knob.
    pub knobs_offset: usize,
    /// How many knobs.
    pub knobs: usize,
    /// Value the knobs wrap at.
    pub knob_max: u16,
    /// Byte offset of the first analog field.
    pub analog_offset: usize,
    /// How many analog fields.
    pub analog_count: usize,
    /// How many pads.
    pub pads: usize,
    /// Full-scale pad reading.
    pub pad_max: u16,
    /// Flat LED slot of pad 0.
    pub pad_led_base: usize,
    /// Flat LED slot of the touch strip's first LED.
    pub strip_led_base: usize,
    /// How many LEDs the strip has.
    pub strip_leds: usize,
    /// Whether the strip's slots run opposite to the finger.
    pub strip_led_reversed: bool,
    /// Which analog field the strip reports on.
    pub strip_analog: usize,
    /// Full-scale strip reading.
    pub strip_max: u16,
    /// First analog field belonging to the pedal jack.
    pub pedal_analog_base: usize,
    /// Panel width, in the units the controls use.
    pub panel_width: f32,
    /// Panel height, in the same units.
    pub panel_height: f32,
}

/// What sort of thing a control is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlKind {
    /// A button in the HID button bitfield.
    Button,
    /// One of the pads.
    Pad,
    /// One of the knobs.
    Knob,
    /// The push encoder's rotation.
    Encoder,
    /// The touch strip.
    Strip,
    /// A display.
    Screen,
}

/// One control on the panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// What sort of control it is.
    pub kind: ControlKind,
    /// Short text for the panel map.
    #[serde(default)]
    pub label: String,
    /// Index within its kind: pad number, knob number.
    #[serde(default)]
    pub index: usize,
    /// Bit in the button field, for a button.
    #[serde(default)]
    pub bit: Option<usize>,
    /// Flat LED slot, when it has a light.
    #[serde(default)]
    pub led: Option<usize>,
    /// Palette index when its LED decodes colour rather than brightness.
    #[serde(default)]
    pub led_colour: Option<u8>,
    /// Loose grouping, for the config file and the panel map.
    #[serde(default)]
    pub group: Option<String>,
    /// Panel position, omitted when unplaced.
    #[serde(default)]
    pub x: Option<f32>,
    /// Panel position.
    #[serde(default)]
    pub y: Option<f32>,
    /// Panel size.
    #[serde(default)]
    pub w: Option<f32>,
    /// Panel size.
    #[serde(default)]
    pub h: Option<f32>,
}

impl Profile {
    /// Parse a profile from TOML.
    pub fn parse(text: &str) -> Result<Self> {
        let p: Profile = toml::from_str(text).context("parsing device profile")?;
        p.validate()?;
        Ok(p)
    }

    /// The compiled-in Maschine MK3 profile.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN_MK3).expect("the shipped profile must parse")
    }

    /// Load `path`, or fall back to the compiled-in profile if it is absent.
    pub fn load_or_builtin(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::builtin()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Where a user-supplied profile would live.
    pub fn default_path() -> PathBuf {
        crate::config::Config::default_path()
            .parent()
            .map(|d| d.join("devices").join("maschine-mk3.toml"))
            .unwrap_or_else(|| PathBuf::from("maschine-mk3.toml"))
    }

    /// Reject a profile the driver could not act on.
    pub fn validate(&self) -> Result<()> {
        let leds = self.report.led_count();
        if self.layout.button_bits % 8 != 0 {
            bail!("layout.button_bits must be a multiple of 8");
        }
        if self.layout.pad_led_base + self.layout.pads > leds {
            bail!("pad LEDs run past the last slot ({leds})");
        }
        if self.layout.strip_led_base + self.layout.strip_leds > leds {
            bail!("touch strip LEDs run past the last slot ({leds})");
        }
        if self.layout.strip_analog >= self.layout.analog_count {
            bail!(
                "layout.strip_analog {} is outside 0..{}",
                self.layout.strip_analog,
                self.layout.analog_count
            );
        }
        if self.layout.knob_max == 0 || self.layout.pad_max == 0 || self.layout.strip_max == 0 {
            bail!("full-scale values must be above 0");
        }

        // The pad and strip blocks are painted wholesale, so they must not
        // overlap each other either.
        let pads = self.layout.pad_led_base..self.layout.pad_led_base + self.layout.pads;
        let strip =
            self.layout.strip_led_base..self.layout.strip_led_base + self.layout.strip_leds;
        if pads.start < strip.end && strip.start < pads.end {
            bail!(
                "the pad block ({}..{}) and the touch strip block ({}..{}) overlap",
                pads.start,
                pads.end,
                strip.start,
                strip.end
            );
        }

        let mut bits: BTreeMap<usize, &str> = BTreeMap::new();
        let mut slots: BTreeMap<usize, &str> = BTreeMap::new();
        for (name, c) in &self.control {
            if c.kind == ControlKind::Button {
                let Some(bit) = c.bit else {
                    bail!("control.{name} is a button but has no bit");
                };
                if bit >= self.layout.button_bits {
                    bail!("control.{name}: bit {bit} outside 0..{}", self.layout.button_bits);
                }
                if let Some(prev) = bits.insert(bit, name) {
                    bail!("control.{name} and control.{prev} both claim bit {bit}");
                }
            }
            if let Some(led) = c.led {
                if led >= leds {
                    bail!("control.{name}: led {led} outside 0..{leds}");
                }
                if let Some(prev) = slots.insert(led, name) {
                    bail!("control.{name} and control.{prev} both claim LED slot {led}");
                }
                // A slot inside a block the driver paints wholesale would be
                // overwritten on every repaint, which is confusing to debug.
                if pads.contains(&led) || strip.contains(&led) {
                    bail!(
                        "control.{name}: LED slot {led} is inside the pad or touch strip block"
                    );
                }
            }
        }
        Ok(())
    }

    /// Every button, by name.
    pub fn buttons(&self) -> impl Iterator<Item = (&String, &Control)> {
        self.control
            .iter()
            .filter(|(_, c)| c.kind == ControlKind::Button)
    }

    /// Look up a control by name.
    pub fn get(&self, name: &str) -> Option<&Control> {
        self.control.get(name)
    }

    /// The button occupying `bit`.
    pub fn button_at_bit(&self, bit: usize) -> Option<(&String, &Control)> {
        self.buttons().find(|(_, c)| c.bit == Some(bit))
    }

    /// The control owning LED slot `led`.
    pub fn control_at_led(&self, led: usize) -> Option<(&String, &Control)> {
        self.control.iter().find(|(_, c)| c.led == Some(led))
    }

    /// Controls that have a panel position, for the map.
    pub fn placed(&self) -> impl Iterator<Item = (&String, &Control)> {
        self.control.iter().filter(|(_, c)| c.x.is_some())
    }

    /// Write the profile back out, preserving the file's comments.
    pub fn save_preserving(&self, path: &Path) -> Result<()> {
        let original = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BUILTIN_MK3.to_string(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut doc: toml_edit::DocumentMut = original.parse().context("parsing profile")?;
        let fresh: toml_edit::DocumentMut =
            toml_edit::ser::to_document(self).context("serialising profile")?;
        for (key, item) in fresh.iter() {
            match doc.get_mut(key) {
                Some(existing) => crate::config::merge_preserving(existing, item),
                None => doc[key] = crate::config::expand_inline_tables(item),
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, doc.to_string())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_profile_is_valid() {
        let p = Profile::builtin();
        assert_eq!(p.device.vendor, 0x17cc);
        assert_eq!(p.report.led_count(), 103);
        assert_eq!(p.layout.pads, 16);
    }

    #[test]
    fn shipped_profile_has_the_whole_panel() {
        let p = Profile::builtin();
        assert_eq!(p.buttons().count(), 72, "every button bit is named");
        let lit = p.buttons().filter(|(_, c)| c.led.is_some()).count();
        assert_eq!(lit, 62, "every button LED slot is accounted for");
        let colour = p.control.values().filter(|c| c.led_colour.is_some()).count();
        assert_eq!(colour, 15);
        for i in 0..16 {
            assert!(
                p.control
                    .values()
                    .any(|c| c.kind == ControlKind::Pad && c.index == i),
                "pad {i} missing"
            );
        }
    }

    #[test]
    fn a_button_led_inside_the_pad_block_is_rejected() {
        let mut p = Profile::builtin();
        let base = p.layout.pad_led_base;
        p.control.get_mut("play").unwrap().led = Some(base + 1);
        assert!(
            p.validate().is_err(),
            "the pad block is repainted wholesale; a button there would flicker"
        );
    }

    #[test]
    fn nothing_falls_outside_the_panel() {
        let p = Profile::builtin();
        for (name, c) in p.placed() {
            let (x, y) = (c.x.unwrap(), c.y.unwrap());
            let (w, h) = (c.w.unwrap_or(0.0), c.h.unwrap_or(0.0));
            assert!(
                x >= 0.0
                    && y >= 0.0
                    && x + w <= p.layout.panel_width
                    && y + h <= p.layout.panel_height,
                "{name} sticks out of the panel"
            );
        }
    }

    #[test]
    fn duplicate_bits_are_rejected() {
        let mut p = Profile::builtin();
        let bit = p.control.get("play").unwrap().bit;
        p.control.get_mut("stop").unwrap().bit = bit;
        assert!(p.validate().is_err());
    }
}
