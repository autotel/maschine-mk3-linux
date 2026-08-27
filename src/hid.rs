//! Decoding the MK3's HID input reports.
//!
//! The layouts below come straight from the device's own report descriptor
//! (`/sys/class/hidraw/hidrawN/device/report_descriptor`), so they are facts
//! about the hardware rather than guesses:
//!
//! Report `0x01`, 41 payload bytes:
//!
//! | offset | size | contents                              |
//! |--------|------|---------------------------------------|
//! | 0      | 10   | 80 button bits                        |
//! | 10     | 1    | two 4-bit counters (encoder)          |
//! | 11     | 16   | 8 knobs, u16 LE, logical range 0..999 |
//! | 27     | 14   | 7 analog fields, u16 LE (see [`ANALOGS`]) |
//!
//! The report descriptor splits those last seven into a 0..65535 group of
//! three and two 0..4095 groups, but they are contiguous in the report and the
//! declared ranges do not match what the hardware sends, so the driver reads
//! them as one block and lets the config say which field is what.
//!
//! Report `0x02`, 63 payload bytes: 21 pad event triplets, terminated early by
//! an all-zero triplet.

/// Number of velocity-sensitive pads.
pub const PADS: usize = 16;
/// Number of continuous knobs under the displays.
pub const KNOBS: usize = 8;
/// Number of button bits the device reports.
pub const BUTTON_BITS: usize = 80;
/// Number of analog fields following the knobs.
///
/// Confirmed by sliding a finger on a real MK3 and watching all seven:
///
/// | index | offset | observed | what |
/// |---|---|---|---|
/// | 0 | 27 | climbs ~16 per report, never resets | a free-running counter, not a control |
/// | 1 | 29 | 0..1024, returns to 0 on release | **the touch strip** |
/// | 2 | 31 | idle | unknown |
/// | 3 | 33 | idle | unknown |
/// | 4-6 | 35-39 | idle | the pedal jack |
///
/// Index 0 moving in step with the strip is why it must not be used as a
/// control source: it would emit a CC on every report forever.
pub const ANALOGS: usize = 7;

/// Analog field carrying the touch strip.
pub const STRIP_ANALOG: usize = 1;
/// Full-scale value the touch strip reports.
pub const STRIP_MAX: u16 = 1024;

/// Full logical value of a knob; the device reports 0..=999.
pub const KNOB_MAX: u16 = 999;
/// Full logical value of a pad hit; the device reports a 12-bit value.
pub const PAD_MAX: u16 = 4095;

/// What a pad event triplet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadEvent {
    /// Finger down without a strike (the pad is being leaned on).
    PressOn,
    /// A struck note, value is strike velocity.
    NoteOn,
    /// Finger lifted after a press.
    PressOff,
    /// Note released.
    NoteOff,
    /// Continuous pressure while held.
    Aftertouch,
}

impl PadEvent {
    fn from_nibble(n: u8) -> Option<Self> {
        Some(match n {
            0x00 => Self::PressOn,
            0x10 => Self::NoteOn,
            0x20 => Self::PressOff,
            0x30 => Self::NoteOff,
            0x40 => Self::Aftertouch,
            _ => return None,
        })
    }

    /// Whether this event starts a note.
    pub fn is_on(self) -> bool {
        matches!(self, Self::NoteOn | Self::PressOn)
    }

    /// Whether this event ends a note.
    pub fn is_off(self) -> bool {
        matches!(self, Self::NoteOff | Self::PressOff)
    }
}

/// A decoded snapshot of report `0x01`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlState {
    /// 80 button bits, LSB of byte 0 is bit 0.
    pub buttons: [u8; 10],
    /// Low nibble of the encoder byte: the main stepped encoder, wraps 0..15.
    pub encoder_lo: u8,
    /// High nibble of the encoder byte.
    pub encoder_hi: u8,
    /// Knob positions, 0..=999.
    pub knobs: [u16; KNOBS],
    /// The seven analog fields following the knobs. See [`ANALOGS`].
    pub analog: [u16; ANALOGS],
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            buttons: [0; 10],
            encoder_lo: 0,
            encoder_hi: 0,
            knobs: [0; KNOBS],
            analog: [0; ANALOGS],
        }
    }
}

impl ControlState {
    /// Whether button `bit` (0..80) is currently held.
    #[inline]
    pub fn button(&self, bit: usize) -> bool {
        bit < BUTTON_BITS && self.buttons[bit / 8] & (1 << (bit % 8)) != 0
    }

    /// Parse report `0x01`'s payload (everything after the report id).
    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < 41 {
            return None;
        }
        let u16le = |o: usize| u16::from_le_bytes([p[o], p[o + 1]]);
        let mut s = Self {
            buttons: p[0..10].try_into().ok()?,
            encoder_lo: p[10] & 0x0f,
            encoder_hi: p[10] >> 4,
            ..Default::default()
        };
        for i in 0..KNOBS {
            s.knobs[i] = u16le(11 + i * 2);
        }
        for i in 0..ANALOGS {
            s.analog[i] = u16le(27 + i * 2);
        }
        Some(s)
    }
}

/// One pad event decoded from report `0x02`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadHit {
    /// Pad index, 0..16.
    pub pad: u8,
    /// What happened.
    pub event: PadEvent,
    /// 12-bit velocity or pressure.
    pub value: u16,
}

/// Decode report `0x02`'s payload into `out`.
///
/// The device packs up to 21 events per report and pads the remainder with
/// zeroes, so decoding stops at the first all-zero triplet past the first slot.
pub fn parse_pads(p: &[u8], out: &mut Vec<PadHit>) {
    out.clear();
    let mut i = 0;
    while i + 2 < p.len() {
        let pad = p[i];
        let raw = p[i + 1];
        let value = ((raw as u16 & 0x0f) << 8) | p[i + 2] as u16;
        if i > 0 && pad == 0 && raw == 0 && value == 0 {
            break;
        }
        if let Some(event) = PadEvent::from_nibble(raw & 0xf0) {
            if (pad as usize) < PADS {
                out.push(PadHit { pad, event, value });
            }
        }
        i += 3;
    }
}

/// Shortest signed path between two readings of a counter that wraps at
/// `modulus`.
///
/// Both the encoder and the knobs are endless: they report a position that
/// rolls over rather than stopping at an end. Subtracting naively makes a
/// single click across the seam read as a near-full-scale jump in the wrong
/// direction, so movement has to be measured the short way round.
#[inline]
pub fn wrap_delta(prev: u16, now: u16, modulus: u16) -> i32 {
    let m = modulus as i32;
    let d = (now as i32 - prev as i32).rem_euclid(m);
    if d > m / 2 {
        d - m
    } else {
        d
    }
}

/// [`wrap_delta`] for the encoder's 4-bit counter.
#[inline]
pub fn nibble_delta(prev: u8, now: u8) -> i8 {
    wrap_delta(prev as u16, now as u16, 16) as i8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_delta_takes_the_short_way_round() {
        // The knobs report 0..999 and roll over. One notch past the seam is a
        // small step, not a 999-unit lurch.
        assert_eq!(wrap_delta(998, 2, 1000), 4);
        assert_eq!(wrap_delta(2, 998, 1000), -4);
        assert_eq!(wrap_delta(100, 140, 1000), 40);
        assert_eq!(wrap_delta(140, 100, 1000), -40);
        assert_eq!(wrap_delta(500, 500, 1000), 0);
    }

    #[test]
    fn encoder_wraps_both_ways() {
        assert_eq!(nibble_delta(15, 0), 1);
        assert_eq!(nibble_delta(0, 15), -1);
        assert_eq!(nibble_delta(3, 5), 2);
        assert_eq!(nibble_delta(5, 3), -2);
        assert_eq!(nibble_delta(4, 4), 0);
    }

    #[test]
    fn pad_report_stops_at_padding() {
        // one real hit, then zero padding
        let mut p = vec![0u8; 63];
        p[0] = 5;
        p[1] = 0x1f;
        p[2] = 0xff;
        let mut out = Vec::new();
        parse_pads(&p, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pad, 5);
        assert_eq!(out[0].event, PadEvent::NoteOn);
        assert_eq!(out[0].value, 4095);
    }

    #[test]
    fn pad_zero_in_first_slot_is_a_real_event() {
        let mut p = vec![0u8; 63];
        p[0] = 0;
        p[1] = 0x30;
        p[2] = 0x00;
        let mut out = Vec::new();
        parse_pads(&p, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].event, PadEvent::NoteOff);
    }

    #[test]
    fn control_state_reads_knobs_little_endian() {
        let mut p = vec![0u8; 41];
        p[0] = 0b0000_0101; // buttons 0 and 2
        p[10] = 0x37;
        p[11] = 0xe7;
        p[12] = 0x03; // knob 0 = 999
        let s = ControlState::parse(&p).unwrap();
        assert!(s.button(0) && !s.button(1) && s.button(2));
        assert_eq!(s.encoder_lo, 7);
        assert_eq!(s.encoder_hi, 3);
        assert_eq!(s.knobs[0], 999);
    }
}
