//! Turning device events into MIDI, and MIDI back into LED state.
//!
//! This runs on the input thread, so it allocates nothing after construction
//! and does no I/O of its own -- messages come out through a callback the
//! caller supplies. Keeping the engine pure also makes it testable without a
//! device attached.

use crate::config::{
    Action, ButtonMode, CompiledButton, Config, KnobMode, LedMode, Pickup,
};
use crate::profile::Profile;
use crate::hid::{self, ControlState, PadEvent, PadHit, KNOBS, PADS};
use crate::leds::{self, Leds, Level};
use crate::midi::Msg;

/// Per-knob soft-takeover and deadband state.
#[derive(Debug, Clone, Copy, Default)]
struct KnobState {
    raw: u16,
    last_sent: u8,
    /// In pickup mode, whether the knob has crossed the host's value.
    engaged: bool,
    seen: bool,
    /// Integrated travel in raw units, for accumulate mode.
    accum: i32,
}

/// Per-pad note bookkeeping, so a note off always matches a note on.
#[derive(Debug, Clone, Copy, Default)]
struct PadState {
    /// Note number currently sounding, if any.
    sounding: Option<u8>,
    /// Last pressure value transmitted.
    pressure: u8,
}

/// The mapping engine.
pub struct Engine {
    cfg: Config,
    profile: Profile,
    buttons: Vec<CompiledButton>,
    /// Fast path from a HID bit to an index into `buttons`; 255 means unmapped.
    bit_to_button: [u8; hid::BUTTON_BITS],
    /// Latched state for toggle-mode buttons.
    toggle: [bool; hid::BUTTON_BITS],
    prev: ControlState,
    have_prev: bool,
    knobs: [KnobState; KNOBS],
    pads: [PadState; PADS],
    encoder_pos: i32,
    /// Highest pad pressure this cycle, for channel aftertouch.
    chan_pressure: u8,
    /// What was last transmitted for each control, for the screens to show.
    outputs: Outputs,
}

/// The values the engine last put on the wire.
///
/// The screens have to show these rather than raw hardware readings: a knob
/// reports an endless 0..999 position that rolls over, so drawing the raw
/// value makes the meter wrap even though the MIDI value is clamped and does
/// not. Displaying anything other than what was sent is a lie about what the
/// host received.
#[derive(Debug, Clone, Copy)]
pub struct Outputs {
    /// Last CC value sent for each knob.
    pub knobs: [u8; KNOBS],
    /// Last CC value sent for the encoder.
    pub encoder: u8,
    /// Last CC value sent for the touch strip.
    pub strip: u8,
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            knobs: [0; KNOBS],
            encoder: 0,
            strip: 0,
        }
    }
}

impl Engine {
    /// Build an engine for `cfg` running on `profile`.
    pub fn new(profile: Profile, cfg: Config) -> Self {
        let buttons = cfg.compiled_buttons(&profile);
        let mut bit_to_button = [255u8; hid::BUTTON_BITS];
        for (i, b) in buttons.iter().enumerate() {
            if b.bit < hid::BUTTON_BITS && i < 255 {
                bit_to_button[b.bit] = i as u8;
            }
        }
        Self {
            cfg,
            profile,
            buttons,
            bit_to_button,
            toggle: [false; hid::BUTTON_BITS],
            prev: ControlState::default(),
            have_prev: false,
            knobs: [KnobState::default(); KNOBS],
            pads: [PadState::default(); PADS],
            encoder_pos: 0,
            chan_pressure: 0,
            outputs: Outputs::default(),
        }
    }

    /// The values the engine last transmitted, for display.
    pub fn outputs(&self) -> &Outputs {
        &self.outputs
    }

    /// Swap in a new config, keeping nothing that could now be stale.
    ///
    /// Toggle latches are cleared deliberately: after an edit the host's idea
    /// of a toggle's state and ours can no longer be reconciled, and silently
    /// inverting a button is worse than resetting it.
    pub fn reload(&mut self, cfg: Config) {
        let keep_prev = self.prev;
        let had_prev = self.have_prev;
        let profile = self.profile.clone();
        *self = Self::new(profile, cfg);
        self.prev = keep_prev;
        self.have_prev = had_prev;
    }

    /// The config currently in force.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The hardware description in force.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Paint the idle LED picture for the current config.
    pub fn paint_idle(&self, leds: &mut Leds) {
        if !self.cfg.leds.enabled {
            leds.all_off();
            return;
        }
        for pad in 0..PADS {
            let slot = self.profile.layout.pad_led_base + pad;
            leds.set(
                slot,
                leds::colour(
                    self.cfg.pads.idle_colour,
                    Level::from_u8(self.cfg.pads.idle_level),
                ),
            );
        }
        self.light_strip(0, leds);
        for b in &self.buttons {
            if b.led < 0 {
                continue;
            }
            let lit = matches!(b.led_mode, LedMode::Always);
            leds.set(b.led as usize, self.button_led(b, lit));
        }
    }

    /// Handle report `0x01`. Emits through `out`, updates `leds`.
    pub fn on_controls(
        &mut self,
        s: &ControlState,
        leds: &mut Leds,
        out: &mut impl FnMut(Msg),
    ) {
        if !self.have_prev {
            // First report only establishes a baseline. Emitting here would
            // fire every held button and every knob position at startup.
            self.prev = *s;
            self.have_prev = true;
            for i in 0..KNOBS {
                // Record where each knob happens to be sitting, but leave
                // `seen` clear so accumulate mode picks up its configured
                // starting value on the first real movement.
                self.knobs[i].raw = s.knobs[i] % (hid::KNOB_MAX + 1);
            }
            self.encoder_pos = self.cfg.encoder.initial as i32;
            self.outputs.encoder = self.cfg.encoder.initial;
            if self.cfg.knobs.mode == KnobMode::Accumulate {
                self.outputs.knobs = [self.cfg.knobs.initial; KNOBS];
            }
            return;
        }

        self.scan_buttons(s, leds, out);
        self.scan_knobs(s, out);
        self.scan_encoder(s, out);
        self.scan_analog(s, leds, out);
        self.prev = *s;
    }

    fn scan_buttons(&mut self, s: &ControlState, leds: &mut Leds, out: &mut impl FnMut(Msg)) {
        for byte in 0..10 {
            let changed = s.buttons[byte] ^ self.prev.buttons[byte];
            if changed == 0 {
                continue;
            }
            let mut bits = changed;
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let bit = byte * 8 + b;
                let down = s.buttons[byte] & (1 << b) != 0;
                self.fire_button(bit, down, leds, out);
            }
        }
    }

    fn fire_button(
        &mut self,
        bit: usize,
        down: bool,
        leds: &mut Leds,
        out: &mut impl FnMut(Msg),
    ) {
        let idx = self.bit_to_button[bit];
        if idx == 255 {
            return;
        }
        let b = &self.buttons[idx as usize];
        let (send, on) = match b.mode {
            ButtonMode::Momentary => (true, down),
            ButtonMode::Trigger => (down, true),
            ButtonMode::Toggle => {
                if down {
                    self.toggle[bit] = !self.toggle[bit];
                    (true, self.toggle[bit])
                } else {
                    (false, self.toggle[bit])
                }
            }
        };
        if send {
            if let Some(m) = b.action.message(on) {
                out(m);
            }
        }
        if self.cfg.leds.enabled && b.led >= 0 && b.led_mode == LedMode::Follow {
            let lit = match b.mode {
                ButtonMode::Toggle => self.toggle[bit],
                _ => down,
            };
            leds.set(b.led as usize, self.button_led(b, lit));
        }
    }

    /// The LED byte for a button, honouring whether its slot is a colour one.
    ///
    /// Colour and monochrome LEDs share the output report but read their byte
    /// completely differently, so a single brightness ramp cannot serve both.
    fn button_led(&self, b: &CompiledButton, lit: bool) -> u8 {
        if b.led_mode == LedMode::Off {
            return 0;
        }
        if b.led_colour >= 0 {
            leds::colour(
                b.led_colour as u8,
                if lit { Level::Bright } else { Level::Dim },
            )
        } else if lit {
            leds::mono(self.cfg.leds.button_active)
        } else {
            leds::mono(self.cfg.leds.button_idle)
        }
    }

    fn scan_knobs(&mut self, s: &ControlState, out: &mut impl FnMut(Msg)) {
        let k = &self.cfg.knobs;
        for i in 0..KNOBS {
            let now = s.knobs[i] % (hid::KNOB_MAX + 1);
            let st = &mut self.knobs[i];
            // The knobs are endless, so movement is the short way round the
            // 0..999 seam rather than a plain subtraction.
            let delta = hid::wrap_delta(st.raw, now, hid::KNOB_MAX + 1);
            if delta.unsigned_abs() < k.deadband as u32 {
                continue;
            }
            let cc = k.ccs.get(i).copied().unwrap_or(0);
            match k.mode {
                KnobMode::Relative => {
                    out(Msg::Cc {
                        ch: k.channel - 1,
                        cc,
                        val: k.relative_format.encode(delta.signum() * ((delta.abs() + 7) / 8)),
                    });
                }
                KnobMode::Accumulate => {
                    let travel = k.travel.max(1) as i32;
                    if !st.seen {
                        st.accum = (k.initial as i32 * travel) / 127;
                    }
                    // Clamping here, rather than on the emitted value, is what
                    // stops the knob wrapping: once it is pinned at an end,
                    // further turning in that direction accumulates nothing to
                    // unwind on the way back.
                    st.accum = (st.accum + delta).clamp(0, travel);
                    let v = ((st.accum * 127) / travel) as u8;
                    if st.seen && v == st.last_sent {
                        st.raw = now;
                        continue;
                    }
                    st.last_sent = v;
                    self.outputs.knobs[i] = v;
                    if k.high_resolution {
                        let v14 = ((st.accum as i64 * 16383) / travel as i64) as u16;
                        out(Msg::Cc { ch: k.channel - 1, cc, val: (v14 >> 7) as u8 });
                        out(Msg::Cc { ch: k.channel - 1, cc: cc + 32, val: (v14 & 0x7f) as u8 });
                    } else {
                        out(Msg::Cc { ch: k.channel - 1, cc, val: v });
                    }
                }
                KnobMode::Absolute => {
                    if k.high_resolution {
                        let v14 = ((now as u32 * 16383) / hid::KNOB_MAX as u32) as u16;
                        out(Msg::Cc { ch: k.channel - 1, cc, val: (v14 >> 7) as u8 });
                        out(Msg::Cc { ch: k.channel - 1, cc: cc + 32, val: (v14 & 0x7f) as u8 });
                    } else {
                        let v = ((now as u32 * 127) / hid::KNOB_MAX as u32) as u8;
                        if k.pickup == Pickup::Pickup && !st.engaged {
                            if st.seen && v != st.last_sent {
                                if (st.last_sent as i32 - v as i32).abs() > 1 {
                                    st.raw = now;
                                    continue;
                                }
                            }
                            st.engaged = true;
                        }
                        if st.seen && v == st.last_sent {
                            st.raw = now;
                            continue;
                        }
                        st.last_sent = v;
                        self.outputs.knobs[i] = v;
                        out(Msg::Cc { ch: k.channel - 1, cc, val: v });
                    }
                }
            }
            st.raw = now;
            st.seen = true;
        }
    }

    fn scan_encoder(&mut self, s: &ControlState, out: &mut impl FnMut(Msg)) {
        let d = hid::nibble_delta(self.prev.encoder_lo, s.encoder_lo) as i32;
        if d == 0 {
            return;
        }
        let e = &self.cfg.encoder;
        let step = d * e.step;
        match e.mode {
            KnobMode::Relative => out(Msg::Cc {
                ch: e.channel - 1,
                cc: e.cc,
                val: e.relative_format.encode(step),
            }),
            // The encoder has no end stops either, so both of the other modes
            // integrate and clamp; "absolute" is kept as a spelling of the
            // same thing because it is the more obvious name to reach for.
            KnobMode::Absolute | KnobMode::Accumulate => {
                let before = self.encoder_pos;
                self.encoder_pos = (self.encoder_pos + step).clamp(0, 127);
                if self.encoder_pos != before {
                    self.outputs.encoder = self.encoder_pos as u8;
                    out(Msg::Cc {
                        ch: e.channel - 1,
                        cc: e.cc,
                        val: self.encoder_pos as u8,
                    });
                }
            }
        }
    }

    fn scan_analog(&mut self, s: &ControlState, leds: &mut Leds, out: &mut impl FnMut(Msg)) {
        let scale = |v: u16| ((v.min(hid::PAD_MAX) as u32 * 127) / hid::PAD_MAX as u32) as u8;

        let t = &self.cfg.touchstrip;
        let src = self.profile.layout.strip_analog.min(self.profile.layout.analog_count - 1);
        if s.analog[src] != self.prev.analog[src] {
            let full = self.profile.layout.strip_max.max(1);
            let v = ((s.analog[src].min(full) as u32 * 127) / full as u32) as u8;
            self.outputs.strip = v;
            if t.enabled {
                out(Msg::Cc {
                    ch: t.channel - 1,
                    cc: t.cc,
                    val: v,
                });
            }
            self.light_strip(s.analog[src], leds);
        }

        let p = &self.cfg.pedal;
        if p.enabled {
            for (i, cc) in p.ccs.iter().enumerate() {
                let idx = self.profile.layout.pedal_analog_base + i;
                if idx >= s.analog.len() || s.analog[idx] == self.prev.analog[idx] {
                    continue;
                }
                let val = if p.switch {
                    if s.analog[idx] >= p.switch_threshold {
                        127
                    } else {
                        0
                    }
                } else {
                    scale(s.analog[idx])
                };
                out(Msg::Cc {
                    ch: p.channel - 1,
                    cc: *cc,
                    val,
                });
            }
        }
    }

    /// Handle report `0x02`.
    pub fn on_pads(&mut self, hits: &[PadHit], leds: &mut Leds, out: &mut impl FnMut(Msg)) {
        use crate::config::Aftertouch;
        let p = &self.cfg.pads;
        let ch = p.channel - 1;
        let mut chan_max = self.chan_pressure;

        for h in hits {
            let pad = h.pad as usize;
            if pad >= PADS {
                continue;
            }
            let note = p.notes.get(pad).copied().unwrap_or(0);

            match h.event {
                PadEvent::NoteOn => {
                    if h.value < p.threshold {
                        continue;
                    }
                    // A retrigger without an intervening off would leave a
                    // stuck note in the host, so close the old one first.
                    if let Some(prev) = self.pads[pad].sounding.take() {
                        out(Msg::NoteOff { ch, note: prev, vel: 0 });
                    }
                    out(Msg::NoteOn {
                        ch,
                        note,
                        vel: p.velocity(h.value),
                    });
                    self.pads[pad].sounding = Some(note);
                    self.light_pad(pad, true, leds);
                }
                PadEvent::PressOn => {
                    self.light_pad(pad, true, leds);
                    if p.press_sends_note && self.pads[pad].sounding.is_none() {
                        out(Msg::NoteOn {
                            ch,
                            note,
                            vel: p.velocity(h.value.max(1)),
                        });
                        self.pads[pad].sounding = Some(note);
                    }
                }
                PadEvent::NoteOff | PadEvent::PressOff => {
                    if let Some(prev) = self.pads[pad].sounding.take() {
                        out(Msg::NoteOff { ch, note: prev, vel: 0 });
                    }
                    if self.pads[pad].pressure != 0 {
                        self.pads[pad].pressure = 0;
                        if p.aftertouch == Aftertouch::Poly {
                            out(Msg::PolyAftertouch { ch, note, val: 0 });
                        }
                    }
                    self.light_pad(pad, false, leds);
                }
                PadEvent::Aftertouch => {
                    // The device sends pressure for a resting finger too, and
                    // sometimes a frame before the note on. Sending key
                    // pressure for a note the host was never told about
                    // confuses samplers, so it is held back until the pad is
                    // actually sounding.
                    if p.aftertouch == Aftertouch::Off || self.pads[pad].sounding.is_none() {
                        continue;
                    }
                    let v = p.pressure(h.value);
                    if v == self.pads[pad].pressure {
                        continue;
                    }
                    self.pads[pad].pressure = v;
                    match p.aftertouch {
                        Aftertouch::Poly => out(Msg::PolyAftertouch { ch, note, val: v }),
                        Aftertouch::Channel => chan_max = chan_max.max(v),
                        Aftertouch::Off => {}
                    }
                }
            }
        }

        if p.aftertouch == Aftertouch::Channel {
            let peak = self.pads.iter().map(|s| s.pressure).max().unwrap_or(0);
            if peak != self.chan_pressure {
                self.chan_pressure = peak;
                out(Msg::ChannelAftertouch { ch, val: peak });
            }
        }
        let _ = chan_max;
    }

    /// Light the touch strip's LEDs up to the finger's position.
    ///
    /// A value of zero means no finger, which leaves the strip dark rather
    /// than lighting its first segment -- otherwise the strip would glow at
    /// one end whenever nothing is touching it.
    fn light_strip(&self, raw: u16, leds: &mut Leds) {
        let t = &self.cfg.touchstrip;
        let l = &self.profile.layout;
        if !self.cfg.leds.enabled || !t.leds || l.strip_leds == 0 {
            return;
        }
        let full = l.strip_max.max(1) as u32;
        let lit = if raw == 0 {
            0
        } else {
            // Round up so any contact lights at least one segment.
            ((raw.min(l.strip_max) as u32 * l.strip_leds as u32 + full - 1) / full) as usize
        };
        // The profile says which way the slots run; the config can flip it if
        // a unit turns out to be wired the other way.
        let reversed = l.strip_led_reversed != t.led_reversed;
        for i in 0..l.strip_leds {
            let slot = if reversed {
                l.strip_led_base + (l.strip_leds - 1 - i)
            } else {
                l.strip_led_base + i
            };
            leds.set(slot, if i < lit { t.led_value } else { 0 });
        }
    }

    fn light_pad(&self, pad: usize, active: bool, leds: &mut Leds) {
        if !self.cfg.leds.enabled {
            return;
        }
        let p = &self.cfg.pads;
        let v = if active {
            leds::colour(p.active_colour, Level::Bright)
        } else {
            leds::colour(p.idle_colour, Level::from_u8(p.idle_level))
        };
        leds.set(self.profile.layout.pad_led_base + pad, v);
    }

    /// Apply a MIDI message arriving from the host to the LED surface.
    ///
    /// Buttons with `led_mode = "midi"` light when the host sends a non-zero
    /// value on their own address, which is how a DAW drives a transport LED.
    pub fn on_host_midi(&mut self, m: Msg, leds: &mut Leds) {
        if !self.cfg.leds.enabled {
            return;
        }
        let (ch, kind, num, val) = match m {
            Msg::Cc { ch, cc, val } => (ch, 0u8, cc, val),
            Msg::NoteOn { ch, note, vel } => (ch, 1, note, vel),
            Msg::NoteOff { ch, note, .. } => (ch, 1, note, 0),
            _ => return,
        };

        for b in &self.buttons {
            if b.led < 0 || b.led_mode != LedMode::Midi {
                continue;
            }
            let hit = match b.action {
                Action::Cc { ch: c, cc, .. } => kind == 0 && c == ch && cc == num,
                Action::Note { ch: c, note, .. } => kind == 1 && c == ch && note == num,
                _ => false,
            };
            if hit {
                leds.set(b.led as usize, self.button_led(b, val > 0));
            }
        }

        // Notes on the pad channel light the matching pad, so a sequencer
        // playing back shows up on the grid.
        if kind == 1 && ch == self.cfg.pads.channel - 1 {
            if let Some(pad) = self.cfg.pads.notes.iter().position(|&n| n == num) {
                self.light_pad(pad, val > 0, leds);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ButtonCfg, PadCfg};

    fn engine_with(cfg: Config) -> (Engine, Leds) {
        (Engine::new(crate::profile::Profile::builtin(), cfg), Leds::new())
    }

    /// Bind a real control from the shipped profile, so tests exercise the
    /// same name-to-bit join the driver does.
    fn bind(cfg: &mut Config, name: &str, send: &str, mode: ButtonMode) {
        cfg.buttons.insert(
            name.into(),
            crate::config::Binding::Full(crate::config::ButtonCfg {
                send: send.into(),
                mode,
                led: LedMode::Follow,
            }),
        );
    }

    /// The bit the profile gives a control.
    fn bit_of(name: &str) -> usize {
        crate::profile::Profile::builtin()
            .get(name)
            .and_then(|c| c.bit)
            .unwrap_or_else(|| panic!("`{name}` is not in the shipped profile"))
    }

    fn collect(e: &mut Engine, s: &ControlState, leds: &mut Leds) -> Vec<Msg> {
        let mut v = Vec::new();
        e.on_controls(s, leds, &mut |m| v.push(m));
        v
    }

    #[test]
    fn first_report_is_a_baseline_and_emits_nothing() {
        let mut cfg = Config::default();
        bind(&mut cfg, "play", "cc 1 118", ButtonMode::Momentary);
        let (mut e, mut leds) = engine_with(cfg);
        let mut s = ControlState::default();
        s.buttons[bit_of("play") / 8] = 1 << (bit_of("play") % 8); // held at startup
        assert!(collect(&mut e, &s, &mut leds).is_empty());
    }

    #[test]
    fn momentary_button_sends_on_then_off() {
        let mut cfg = Config::default();
        bind(&mut cfg, "play", "cc 1 118", ButtonMode::Momentary);
        let (mut e, mut leds) = engine_with(cfg);
        let base = ControlState::default();
        collect(&mut e, &base, &mut leds);

        let bit = bit_of("play");
        let mut down = base;
        down.buttons[bit / 8] = 1 << (bit % 8);
        assert_eq!(
            collect(&mut e, &down, &mut leds),
            vec![Msg::Cc { ch: 0, cc: 118, val: 127 }]
        );
        assert_eq!(
            collect(&mut e, &base, &mut leds),
            vec![Msg::Cc { ch: 0, cc: 118, val: 0 }]
        );
    }

    #[test]
    fn colour_button_leds_do_not_use_the_brightness_ramp() {
        // Writing a modest brightness to a colour LED selects a palette entry
        // instead: 10 is (2 << 2) | 2, which comes out orange. Sampling is a
        // colour LED and did exactly that before this existed.
        let mut cfg = Config::default();
        cfg.leds.button_idle = 10;
        // Sampling is a colour LED on real hardware; Mixer is monochrome.
        bind(&mut cfg, "sampling", "none", ButtonMode::Momentary);
        bind(&mut cfg, "mixer", "none", ButtonMode::Momentary);
        let profile = crate::profile::Profile::builtin();
        let sampling = profile.get("sampling").unwrap();
        let mixer = profile.get("mixer").unwrap();
        let (sampling_led, palette) = (sampling.led.unwrap(), sampling.led_colour.unwrap());
        let mixer_led = mixer.led.unwrap();
        assert!(mixer.led_colour.is_none());

        let (e, mut leds) = engine_with(cfg);
        e.paint_idle(&mut leds);
        assert_eq!(
            leds.get(sampling_led),
            crate::leds::colour(palette, crate::leds::Level::Dim),
            "a colour LED idles at its palette entry, dimmed"
        );
        assert_eq!(
            leds.get(mixer_led),
            10,
            "a monochrome one still takes plain brightness"
        );
    }

    #[test]
    fn colour_button_led_brightens_on_press() {
        let mut cfg = Config::default();
        bind(&mut cfg, "sampling", "none", ButtonMode::Momentary);
        let profile = crate::profile::Profile::builtin();
        let c = profile.get("sampling").unwrap();
        let (bit, slot, palette) = (c.bit.unwrap(), c.led.unwrap(), c.led_colour.unwrap());

        let (mut e, mut leds) = engine_with(cfg);
        let base = ControlState::default();
        collect(&mut e, &base, &mut leds);
        let mut down = base;
        down.buttons[bit / 8] = 1 << (bit % 8);
        collect(&mut e, &down, &mut leds);
        assert_eq!(
            leds.get(slot),
            crate::leds::colour(palette, crate::leds::Level::Bright)
        );
    }

    #[test]
    fn toggle_button_ignores_release() {
        let mut cfg = Config::default();
        bind(&mut cfg, "mute", "cc 1 20", ButtonMode::Toggle);
        let (mut e, mut leds) = engine_with(cfg);
        let base = ControlState::default();
        collect(&mut e, &base, &mut leds);
        let bit = bit_of("mute");
        let mut down = base;
        down.buttons[bit / 8] = 1 << (bit % 8);

        assert_eq!(collect(&mut e, &down, &mut leds), vec![Msg::Cc { ch: 0, cc: 20, val: 127 }]);
        assert!(collect(&mut e, &base, &mut leds).is_empty());
        assert_eq!(collect(&mut e, &down, &mut leds), vec![Msg::Cc { ch: 0, cc: 20, val: 0 }]);
    }

    #[test]
    fn pad_retrigger_closes_the_previous_note() {
        let cfg = Config { pads: PadCfg { threshold: 0, ..Default::default() }, ..Default::default() };
        let (mut e, mut leds) = engine_with(cfg);
        let mut msgs = Vec::new();
        let hit = PadHit { pad: 0, event: PadEvent::NoteOn, value: 4095 };
        e.on_pads(&[hit], &mut leds, &mut |m| msgs.push(m));
        e.on_pads(&[hit], &mut leds, &mut |m| msgs.push(m));
        // HID pad 0 is the top-left pad, which the default map puts at 48.
        assert_eq!(
            msgs,
            vec![
                Msg::NoteOn { ch: 9, note: 48, vel: 127 },
                Msg::NoteOff { ch: 9, note: 48, vel: 0 },
                Msg::NoteOn { ch: 9, note: 48, vel: 127 },
            ]
        );
    }

    #[test]
    fn note_off_uses_the_note_that_was_sounding_after_a_remap() {
        let (mut e, mut leds) = engine_with(Config::default());
        let mut msgs = Vec::new();
        e.on_pads(
            &[PadHit { pad: 0, event: PadEvent::NoteOn, value: 2000 }],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        let mut cfg = Config::default();
        cfg.pads.notes[0] = 99;
        e.reload(cfg);
        msgs.clear();
        e.on_pads(
            &[PadHit { pad: 0, event: PadEvent::NoteOff, value: 0 }],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        // A reload resets pad bookkeeping, so nothing is claimed to be sounding.
        assert!(msgs.is_empty(), "must not invent a note off for a note it never sent");
    }

    #[test]
    fn pads_below_threshold_are_dropped() {
        let cfg = Config { pads: PadCfg { threshold: 500, ..Default::default() }, ..Default::default() };
        let (mut e, mut leds) = engine_with(cfg);
        let mut msgs = Vec::new();
        e.on_pads(
            &[PadHit { pad: 2, event: PadEvent::NoteOn, value: 400 }],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        assert!(msgs.is_empty());
    }

    #[test]
    fn strip_meter_fills_from_the_far_end_and_clears_at_rest() {
        let mut cfg = Config::default();
        cfg.touchstrip.led_reversed = true;
        cfg.touchstrip.led_value = 0x1f;
        let layout = crate::profile::Profile::builtin().layout;
        let (base, n) = (layout.strip_led_base, layout.strip_leds);
        let (mut e, mut leds) = engine_with(cfg);

        let src = crate::profile::Profile::builtin().layout.strip_analog;
        let mut s = ControlState::default();
        collect(&mut e, &s, &mut leds);

        // Any contact at all must light at least one segment.
        s.analog[src] = 1;
        collect(&mut e, &s, &mut leds);
        assert_eq!(leds.get(base + n - 1), 0x1f, "reversed: first segment is the last slot");
        assert_eq!(leds.get(base), 0);

        s.analog[src] = crate::profile::Profile::builtin().layout.strip_max;
        collect(&mut e, &s, &mut leds);
        assert!((base..base + n).all(|i| leds.get(i) == 0x1f), "full deflection lights the strip");

        s.analog[src] = 0;
        collect(&mut e, &s, &mut leds);
        assert!((base..base + n).all(|i| leds.get(i) == 0), "no finger leaves the strip dark");
    }

    #[test]
    fn pad_leds_land_on_the_pad_block_not_the_buttons() {
        let cfg = Config::default();
        let layout = crate::profile::Profile::builtin().layout;
        assert_eq!(layout.pad_led_base, 87);
        let (strip, strip_n) = (layout.strip_led_base, layout.strip_leds);
        let (mut e, mut leds) = engine_with(cfg);
        let mut msgs = Vec::new();
        e.on_pads(
            &[PadHit { pad: 0, event: PadEvent::NoteOn, value: 4095 }],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        assert_ne!(leds.get(layout.pad_led_base), 0, "pad 0 lights its own slot");
        assert_eq!(leds.get(0), 0, "and must not touch the button slots");
        assert!(
            (strip..strip + strip_n).all(|i| leds.get(i) == 0),
            "nor bleed onto the touch strip"
        );
    }

    #[test]
    fn overlapping_pad_and_strip_leds_are_rejected() {
        // Overlap is now a property of the hardware description, so the
        // profile is what refuses it.
        let mut p = crate::profile::Profile::builtin();
        p.layout.strip_leds = 40; // 62..102 runs into the pad block at 87
        assert!(p.validate().is_err());
    }

    #[test]
    fn strip_reaches_full_range_at_its_own_scale() {
        // The strip tops out at 1024, not the pads' 4095. Scaling against the
        // wrong full-scale value would cap the CC at about 31.
        let (mut e, mut leds) = engine_with(Config::default());
        let src = crate::profile::Profile::builtin().layout.strip_analog;
        let mut s = ControlState::default();
        collect(&mut e, &s, &mut leds);
        s.analog[src] = crate::profile::Profile::builtin().layout.strip_max;
        let msgs = collect(&mut e, &s, &mut leds);
        assert!(
            msgs.contains(&Msg::Cc { ch: 0, cc: 1, val: 127 }),
            "full deflection must reach 127, got {msgs:?}"
        );
    }

    #[test]
    fn free_running_counter_is_not_an_allowed_strip_source() {
        // Which field the strip sits on is hardware, so the profile owns it.
        let p = crate::profile::Profile::builtin();
        assert_ne!(
            p.layout.strip_analog, 0,
            "field 0 climbs forever; using it would emit a CC on every report"
        );
    }

    #[test]
    fn aftertouch_is_withheld_until_the_pad_is_sounding() {
        let (mut e, mut leds) = engine_with(Config::default());
        let mut msgs = Vec::new();
        // The device really does send pressure before the note on.
        e.on_pads(
            &[PadHit { pad: 7, event: PadEvent::Aftertouch, value: 900 }],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        assert!(msgs.is_empty(), "no key pressure for a note the host never got");

        e.on_pads(
            &[
                PadHit { pad: 7, event: PadEvent::NoteOn, value: 1500 },
                PadHit { pad: 7, event: PadEvent::Aftertouch, value: 900 },
            ],
            &mut leds,
            &mut |m| msgs.push(m),
        );
        assert!(
            msgs.iter().any(|m| matches!(m, Msg::PolyAftertouch { .. })),
            "but it flows once the note is sounding: {msgs:?}"
        );
    }

    #[test]
    fn velocity_scales_against_what_the_pads_actually_reach() {
        let c = Config::default();
        // Measured on hardware: a hard hit lands near 1950, ordinary playing
        // between 200 and 1400.
        assert_eq!(c.pads.velocity(1950), 124, "a hard hit is nearly full");
        assert!(
            (55..=75).contains(&c.pads.velocity(1000)),
            "a medium hit sits mid-scale, got {}",
            c.pads.velocity(1000)
        );
        assert_eq!(c.pads.velocity(4095), 127, "and it saturates rather than wrapping");
    }

    #[test]
    fn knob_deadband_suppresses_jitter() {
        let mut cfg = Config::default();
        cfg.knobs.deadband = 5;
        let (mut e, mut leds) = engine_with(cfg);
        let mut s = ControlState::default();
        s.knobs[0] = 500;
        collect(&mut e, &s, &mut leds);
        s.knobs[0] = 502;
        assert!(collect(&mut e, &s, &mut leds).is_empty());
        s.knobs[0] = 520;
        assert!(!collect(&mut e, &s, &mut leds).is_empty());
    }

    #[test]
    fn knobs_clamp_at_the_ends_instead_of_wrapping() {
        // The knobs are endless encoders reporting 0..999. Turning up past the
        // top must stay at 127, not roll over to 0.
        let (mut e, mut leds) = engine_with(Config::default());
        let mut s = ControlState::default();
        s.knobs[0] = 500;
        collect(&mut e, &s, &mut leds);

        let mut last = 0u8;
        // Two full turns' worth of upward motion, crossing the 999/0 seam.
        for step in 1..=40 {
            s.knobs[0] = (500 + step * 50) % 1000;
            for m in collect(&mut e, &s, &mut leds) {
                if let Msg::Cc { val, .. } = m {
                    assert!(val >= last, "value went backwards across the seam: {last} -> {val}");
                    last = val;
                }
            }
        }
        assert_eq!(last, 127, "should have pinned at the top");

        // And it comes straight back down, rather than needing to unwind the
        // travel that was clamped away.
        s.knobs[0] = (s.knobs[0] + 1000 - 50) % 1000;
        let msgs = collect(&mut e, &s, &mut leds);
        match msgs.first() {
            Some(Msg::Cc { val, .. }) => assert!(*val < 127 && *val > 100, "got {val}"),
            other => panic!("expected an immediate decrease, got {other:?}"),
        }
    }

    #[test]
    fn knob_starts_from_its_configured_position() {
        let (mut e, mut leds) = engine_with(Config::default());
        let mut s = ControlState::default();
        s.knobs[0] = 900;
        collect(&mut e, &s, &mut leds);
        s.knobs[0] = 950;
        let msgs = collect(&mut e, &s, &mut leds);
        match msgs.first() {
            // 64 is the configured start; +50 raw of 1000 travel is +6.
            Some(Msg::Cc { val, .. }) => assert_eq!(*val, 70),
            other => panic!("expected a step up from centre, got {other:?}"),
        }
    }

    #[test]
    fn encoder_clamps_and_is_silent_at_the_ends() {
        let mut cfg = Config::default();
        cfg.encoder.step = 2;
        let (mut e, mut leds) = engine_with(cfg);
        let mut s = ControlState::default();
        collect(&mut e, &s, &mut leds);

        let mut last = 64;
        for i in 1..200 {
            s.encoder_lo = (i % 16) as u8;
            for m in collect(&mut e, &s, &mut leds) {
                if let Msg::Cc { val, .. } = m {
                    last = val;
                }
            }
        }
        assert_eq!(last, 127, "turning up reaches the top");

        // Once pinned, more turning in the same direction says nothing.
        let before = s.encoder_lo;
        s.encoder_lo = (before + 1) % 16;
        assert!(
            collect(&mut e, &s, &mut leds).is_empty(),
            "a pinned encoder must not keep resending 127"
        );
    }

    #[test]
    fn toggle_button_led_shows_the_latched_state() {
        let mut cfg = Config::default();
        cfg.leds.button_idle = 5;
        cfg.leds.button_active = 120;
        bind(&mut cfg, "mute", "cc 16 37", ButtonMode::Toggle);
        // Bit and LED slot both come from the profile, so the test stays true
        // if the hardware map is ever corrected.
        let profile = crate::profile::Profile::builtin();
        let c = profile.get("mute").unwrap();
        let (bit, slot) = (c.bit.unwrap(), c.led.unwrap());
        assert!(c.led_colour.is_none(), "mute is monochrome");

        let (mut e, mut leds) = engine_with(cfg);
        let base = ControlState::default();
        collect(&mut e, &base, &mut leds);
        let mut down = base;
        down.buttons[bit / 8] = 1 << (bit % 8);

        collect(&mut e, &down, &mut leds);
        assert_eq!(leds.get(slot), 120, "latched on");
        collect(&mut e, &base, &mut leds);
        assert_eq!(leds.get(slot), 120, "and stays lit through the release");
        collect(&mut e, &down, &mut leds);
        assert_eq!(leds.get(slot), 5, "second press latches off");
    }

    #[test]
    fn encoder_wrap_reads_as_one_step() {
        let mut cfg = Config::default();
        cfg.encoder.mode = KnobMode::Relative;
        cfg.encoder.step = 1;
        let (mut e, mut leds) = engine_with(cfg);
        let mut s = ControlState::default();
        s.encoder_lo = 15;
        collect(&mut e, &s, &mut leds);
        s.encoder_lo = 0;
        assert_eq!(
            collect(&mut e, &s, &mut leds),
            vec![Msg::Cc { ch: 0, cc: 24, val: 1 }]
        );
    }
}
