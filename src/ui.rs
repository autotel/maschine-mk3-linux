//! What the two screens show.
//!
//! The default surface is a readout of the eight knobs -- four per screen --
//! plus a header naming the device and the pad channel. It exists so the
//! screens are informative rather than dark; the layout deliberately stays
//! simple, because every redraw competes with input for USB bandwidth.

use crate::config::Config;
use crate::display::{font, rgb, Frame, H, W};
use crate::engine::Outputs;
use crate::hid::KNOBS;

/// Colours used by the default surface.
mod palette {
    use crate::display::rgb;

    /// Page background.
    pub const BG: u16 = rgb(8, 8, 10);
    /// Header background.
    pub const HEADER: u16 = rgb(24, 26, 32);
    /// Primary text.
    pub const FG: u16 = rgb(235, 235, 240);
    /// Secondary text.
    pub const MUTED: u16 = rgb(120, 124, 134);
    /// Filled part of a value bar.
    pub const BAR: u16 = rgb(255, 138, 0);
    /// Empty part of a value bar.
    pub const BAR_BG: u16 = rgb(40, 42, 48);
    /// Divider between knob cells.
    pub const RULE: u16 = rgb(48, 50, 58);
}

/// Height of the header strip.
const HEADER_H: usize = 26;
/// Width of one knob cell.
const CELL_W: usize = W / 4;

/// Renders the default surface and tracks what it last drew.
pub struct Surface {
    last: [u8; KNOBS],
    last_valid: bool,
    header_drawn: [bool; 2],
    status: String,
    status_drawn: bool,
}

impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}

impl Surface {
    /// A surface that will paint everything on its first update.
    pub fn new() -> Self {
        Self {
            last: [u8::MAX; KNOBS],
            last_valid: false,
            header_drawn: [false; 2],
            status: String::new(),
            status_drawn: false,
        }
    }

    /// Force a full repaint, after a config reload or a device reset.
    pub fn invalidate(&mut self) {
        self.last_valid = false;
        self.header_drawn = [false; 2];
        self.status_drawn = false;
    }

    /// Replace the status line shown at the bottom of the right screen.
    pub fn set_status(&mut self, s: &str) {
        if self.status != s {
            self.status.clear();
            self.status.push_str(s);
            self.status_drawn = false;
        }
    }

    /// Redraw whatever changed. `left` and `right` are only dirtied where needed.
    ///
    /// Driven by what the engine last transmitted, not by raw hardware
    /// readings: the knobs are endless and their raw position rolls over, so a
    /// meter drawn from it would wrap while the MIDI value it claims to show
    /// stays pinned at the end.
    pub fn update(&mut self, cfg: &Config, out: &Outputs, left: &mut Frame, right: &mut Frame) {
        for (i, frame) in [&mut *left, &mut *right].into_iter().enumerate() {
            if !self.header_drawn[i] {
                frame.clear(palette::BG);
                frame.rect(0, 0, W, HEADER_H, palette::HEADER);
                let title = if i == 0 {
                    cfg.display.title.clone()
                } else {
                    format!("PADS CH {}  KNOBS CH {}", cfg.pads.channel, cfg.knobs.channel)
                };
                frame.text(10, 5, &font::SMALL, 1, palette::FG, &title);
                for c in 1..4 {
                    frame.rect(c * CELL_W, HEADER_H, 1, H - HEADER_H, palette::RULE);
                }
                self.header_drawn[i] = true;
            }
        }

        for k in 0..KNOBS {
            let v = out.knobs[k].min(127);
            if self.last_valid && self.last[k] == v {
                continue;
            }
            self.last[k] = v;
            let frame = if k < 4 { &mut *left } else { &mut *right };
            draw_knob(frame, cfg, k, v);
        }
        self.last_valid = true;

        if !self.status_drawn {
            let y = H - 20;
            right.rect(0, y, W, 20, palette::BG);
            right.text(10, y + 2, &font::SMALL, 1, palette::MUTED, &self.status);
            self.status_drawn = true;
        }
    }
}

fn draw_knob(frame: &mut Frame, cfg: &Config, k: usize, value: u8) {
    let col = k % 4;
    let x = col * CELL_W + 1;
    let w = CELL_W - 2;
    let y = HEADER_H;
    let h = H - HEADER_H - 24;

    frame.rect(x, y, w, h, palette::BG);

    let cc = cfg.knobs.ccs.get(k).copied().unwrap_or(0);
    frame.text_centred(x, w, y + 8, &font::SMALL, 1, palette::MUTED, &format!("CC {cc}"));

    frame.text_centred(
        x,
        w,
        y + 34,
        &font::LARGE,
        1,
        palette::FG,
        &format!("{value:>3}"),
    );

    // Value bar along the bottom of the cell.
    let bar_x = x + 12;
    let bar_w = w.saturating_sub(24);
    let bar_y = y + h - 26;
    frame.rect(bar_x, bar_y, bar_w, 12, palette::BAR_BG);
    let fill = (bar_w * value as usize) / 127;
    if fill > 0 {
        frame.rect(bar_x, bar_y, fill, 12, palette::BAR);
    }
    frame.frame_rect(bar_x, bar_y, bar_w, 12, palette::RULE);
}

/// Paint a startup card while the driver comes up.
pub fn splash(frame: &mut Frame, line1: &str, line2: &str) {
    frame.clear(rgb(8, 8, 10));
    frame.text_centred(0, W, H / 2 - 40, &font::LARGE, 1, rgb(255, 138, 0), line1);
    frame.text_centred(0, W, H / 2 + 8, &font::SMALL, 1, rgb(160, 164, 174), line2);
}
