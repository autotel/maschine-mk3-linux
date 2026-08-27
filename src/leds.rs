//! LED state for pads, buttons and the touch strip.
//!
//! The device takes two output reports, both of them plain arrays of 0..=127
//! brightness/colour bytes:
//!
//! * report `0x80`, 62 bytes
//! * report `0x81`, 41 bytes
//!
//! The driver treats them as one flat address space of
//! [`LED_COUNT`] slots so that the config file can name a slot by a single
//! index and stay agnostic about which report carries it.
//!
//! Colour LEDs encode as `(palette_index << 2) | level`, where `level` is
//! 0..=3. The palette itself lives in writable feature reports `0xfe` / `0xff`,
//! which hold RGB triplets at four brightness steps each -- so a config can
//! redefine what "orange" means rather than being stuck with NI's ramp.

use anyhow::Result;

use crate::device::HidDev;

/// Slots carried by output report `0x80`.
pub const BANK0_LEN: usize = 62;
/// Slots carried by output report `0x81`.
pub const BANK1_LEN: usize = 41;
/// Total addressable LED slots.
pub const LED_COUNT: usize = BANK0_LEN + BANK1_LEN;

/// Brightness steps a colour LED can take, as encoded in the low two bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Fully extinguished.
    Off = 0,
    /// Lowest lit step, used for "this pad exists" backlighting.
    Dim = 1,
    /// Normal step.
    Normal = 2,
    /// Brightest step.
    Bright = 3,
}

impl Level {
    /// Map 0..=3 onto a level, saturating.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            1 => Self::Dim,
            2 => Self::Normal,
            _ => Self::Bright,
        }
    }
}

/// Encode a colour LED byte from a palette index and a brightness step.
#[inline]
pub const fn colour(palette_index: u8, level: Level) -> u8 {
    if level as u8 == 0 {
        0
    } else {
        (palette_index << 2) | (level as u8)
    }
}

/// Encode a monochrome LED byte. Mono LEDs use the full 0..=127 range.
#[inline]
pub const fn mono(brightness: u8) -> u8 {
    if brightness > 127 {
        127
    } else {
        brightness
    }
}

/// The full LED surface, with change tracking so idle frames cost nothing.
pub struct Leds {
    state: [u8; LED_COUNT],
    sent: [u8; LED_COUNT],
    dirty: bool,
}

impl Default for Leds {
    fn default() -> Self {
        Self::new()
    }
}

impl Leds {
    /// All LEDs dark, and marked dirty so the first flush pushes the state.
    pub fn new() -> Self {
        Self {
            state: [0; LED_COUNT],
            // A value the device can never hold makes the first flush unconditional.
            sent: [0xff; LED_COUNT],
            dirty: true,
        }
    }

    /// Set one slot's raw byte.
    #[inline]
    pub fn set(&mut self, index: usize, value: u8) {
        if index < LED_COUNT && self.state[index] != value {
            self.state[index] = value;
            self.dirty = true;
        }
    }

    /// Read back one slot's raw byte.
    #[inline]
    pub fn get(&self, index: usize) -> u8 {
        self.state.get(index).copied().unwrap_or(0)
    }

    /// Turn every LED off.
    pub fn all_off(&mut self) {
        if self.state.iter().any(|&b| b != 0) {
            self.state.fill(0);
            self.dirty = true;
        }
    }

    /// Whether a flush would send anything.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Write both output reports if anything changed.
    ///
    /// Both reports go out together: they are only ~100 bytes over an interrupt
    /// endpoint, and splitting them by bank would double the syscall count for
    /// no benefit.
    pub fn flush(&mut self, dev: &mut HidDev) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut b0 = [0u8; 1 + BANK0_LEN];
        b0[0] = 0x80;
        b0[1..].copy_from_slice(&self.state[..BANK0_LEN]);
        dev.write_report(&b0)?;

        let mut b1 = [0u8; 1 + BANK1_LEN];
        b1[0] = 0x81;
        b1[1..].copy_from_slice(&self.state[BANK0_LEN..]);
        dev.write_report(&b1)?;

        self.sent = self.state;
        self.dirty = false;
        Ok(())
    }

    /// Record that the current state has been handed to whoever writes it out.
    ///
    /// Used when a second thread owns the device handle: the producer clears
    /// its dirty flag here, and the consumer keeps its own copy for
    /// deduplication.
    pub fn mark_published(&mut self) {
        self.sent = self.state;
        self.dirty = false;
    }

    /// Raw state, for the learn tool.
    pub fn raw_mut(&mut self) -> &mut [u8; LED_COUNT] {
        self.dirty = true;
        &mut self.state
    }
}

/// An RGB colour ramp for one palette entry: four brightness steps.
#[derive(Debug, Clone, Copy)]
pub struct Ramp(pub [(u8, u8, u8); 4]);

/// Overwrite the device's colour palette (feature report `0xfe` or `0xff`).
///
/// The report is a flat run of 7-bit RGB triplets, four consecutive triplets
/// per palette entry (one per brightness step). Entries beyond what `ramps`
/// supplies are left black.
pub fn write_palette(dev: &mut HidDev, report_id: u8, ramps: &[Ramp]) -> Result<()> {
    /// Payload length declared by the report descriptor for 0xfe / 0xff.
    const PALETTE_LEN: usize = 208;
    let mut buf = [0u8; 1 + PALETTE_LEN];
    buf[0] = report_id;
    for (i, ramp) in ramps.iter().enumerate() {
        for (j, (r, g, b)) in ramp.0.iter().enumerate() {
            let off = 1 + (i * 4 + j) * 3;
            if off + 2 >= buf.len() {
                break;
            }
            buf[off] = r & 0x7f;
            buf[off + 1] = g & 0x7f;
            buf[off + 2] = b & 0x7f;
        }
    }
    dev.set_feature(&buf)
}

/// Read the device's current colour palette back as ramps.
pub fn read_palette(dev: &mut HidDev, report_id: u8) -> Result<Vec<Ramp>> {
    const PALETTE_LEN: usize = 208;
    let mut buf = [0u8; 1 + PALETTE_LEN];
    buf[0] = report_id;
    let got = dev.get_feature(&mut buf)?.to_vec();
    let body = &got[1..];
    let mut out = Vec::new();
    for chunk in body.chunks_exact(12) {
        let mut r = [(0u8, 0u8, 0u8); 4];
        for (j, t) in chunk.chunks_exact(3).enumerate() {
            r[j] = (t[0], t[1], t[2]);
        }
        out.push(Ramp(r));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_off_is_zero_regardless_of_index() {
        assert_eq!(colour(9, Level::Off), 0);
        assert_eq!(colour(0, Level::Bright), 3);
        assert_eq!(colour(3, Level::Normal), 0b1110);
    }

    #[test]
    fn led_writes_are_deduplicated() {
        let mut l = Leds::new();
        l.dirty = false;
        l.set(4, 0);
        assert!(!l.is_dirty(), "writing the value already held must not dirty");
        l.set(4, 7);
        assert!(l.is_dirty());
    }
}
