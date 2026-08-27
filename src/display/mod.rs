//! The two 480x272 colour displays.
//!
//! Pixels do not go over HID. USB interface 5 ("Maschine MK3 BD") is a
//! vendor-specific bulk endpoint that takes a small command stream:
//!
//! ```text
//! 84 00 SS 60 00 00 00 00     header, SS = display index (0 = left, 1 = right)
//! xxxx yyyy wwww hhhh         destination rect, big-endian u16
//! 02 00 00 00                 command: pixel data follows
//! 0000 llll                   payload length in 32-bit words (w*h/2)
//! <w*h RGB565 pixels, big-endian>
//! 02 00 00 00                 command: end of pixel data
//! 03 00 00 00                 command: blit
//! 40 00 00 00                 command: end of frame
//! ```
//!
//! The kernel binds no driver to interface 5, so claiming it needs no unbind
//! dance -- only read/write permission on the usbfs node, which the shipped
//! udev rule grants.

pub mod font;

use anyhow::{anyhow, Context, Result};
use nusb::transfer::TransferError;
use nusb::Interface;

use crate::{PID, VID};

/// Width of each display in pixels.
pub const W: usize = 480;
/// Height of each display in pixels.
pub const H: usize = 272;
/// Pixels per display.
pub const PIXELS: usize = W * H;

/// USB interface carrying bulk pixel data.
const BD_INTERFACE: u8 = 5;
/// Bulk OUT endpoint on that interface.
const BD_ENDPOINT: u8 = 0x04;

/// An RGB565 framebuffer for one display, with a dirty-row range.
///
/// Tracking dirty rows rather than a full rect keeps the blit logic trivial
/// while still avoiding a 261 KB transfer when only a value readout changed.
pub struct Frame {
    px: Vec<u16>,
    dirty_lo: usize,
    dirty_hi: usize,
}

impl Default for Frame {
    fn default() -> Self {
        Self::new()
    }
}

impl Frame {
    /// A black frame, initially fully dirty so the first flush paints everything.
    pub fn new() -> Self {
        Self {
            px: vec![0; PIXELS],
            dirty_lo: 0,
            dirty_hi: H,
        }
    }

    /// Mark the whole frame for retransmission.
    pub fn touch_all(&mut self) {
        self.dirty_lo = 0;
        self.dirty_hi = H;
    }

    /// True when nothing has changed since the last flush.
    pub fn is_clean(&self) -> bool {
        self.dirty_lo >= self.dirty_hi
    }

    fn mark(&mut self, y0: usize, y1: usize) {
        self.dirty_lo = self.dirty_lo.min(y0);
        self.dirty_hi = self.dirty_hi.max(y1.min(H));
    }

    /// Set one pixel. Out-of-bounds coordinates are ignored.
    #[inline]
    pub fn put(&mut self, x: usize, y: usize, c: u16) {
        if x >= W || y >= H {
            return;
        }
        self.px[y * W + x] = c;
        self.mark(y, y + 1);
    }

    /// Fill the entire frame with one colour.
    pub fn clear(&mut self, c: u16) {
        self.px.fill(c);
        self.touch_all();
    }

    /// Fill an axis-aligned rectangle, clipped to the frame.
    pub fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: u16) {
        let x1 = (x + w).min(W);
        let y1 = (y + h).min(H);
        if x >= W || y >= H || x1 <= x || y1 <= y {
            return;
        }
        for row in y..y1 {
            self.px[row * W + x..row * W + x1].fill(c);
        }
        self.mark(y, y1);
    }

    /// Draw a one-pixel rectangle outline.
    pub fn frame_rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: u16) {
        if w == 0 || h == 0 {
            return;
        }
        self.rect(x, y, w, 1, c);
        self.rect(x, y + h - 1, w, 1, c);
        self.rect(x, y, 1, h, c);
        self.rect(x + w - 1, y, 1, h, c);
    }

    /// Draw `text` at (`x`, `y`) in `font`, scaled `scale`x. Returns the end x.
    pub fn text(&mut self, x: usize, y: usize, font: &font::Font, scale: usize, c: u16, text: &str) -> usize {
        let mut cx = x;
        for ch in text.chars() {
            for gy in 0..font.h {
                for gx in 0..font.w {
                    if !font.pixel(ch, gx, gy) {
                        continue;
                    }
                    if scale == 1 {
                        self.put(cx + gx, y + gy, c);
                    } else {
                        self.rect(cx + gx * scale, y + gy * scale, scale, scale, c);
                    }
                }
            }
            cx += font.w * scale;
            if cx >= W {
                break;
            }
        }
        cx
    }

    /// Draw `text` centred inside the horizontal span [`x`, `x + w`).
    pub fn text_centred(
        &mut self,
        x: usize,
        w: usize,
        y: usize,
        font: &font::Font,
        scale: usize,
        c: u16,
        text: &str,
    ) {
        let tw = text.chars().count() * font.w * scale;
        let sx = if tw >= w { x } else { x + (w - tw) / 2 };
        self.text(sx, y, font, scale, c, text);
    }

    /// Raw pixel access, for callers that render into the buffer themselves.
    pub fn pixels_mut(&mut self) -> &mut [u16] {
        self.touch_all();
        &mut self.px
    }
}

/// Pack 8-bit RGB into RGB565.
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xf8) << 8) | ((g as u16 & 0xfc) << 3) | (b as u16 >> 3)
}

/// Black.
pub const BLACK: u16 = 0;
/// White.
pub const WHITE: u16 = 0xffff;

/// Open bulk connection to both displays.
pub struct Displays {
    iface: Interface,
    buf: Vec<u8>,
}

impl Displays {
    /// Claim interface 5 on the first Maschine MK3 found.
    pub fn open() -> Result<Self> {
        let info = nusb::list_devices()
            .context("enumerating USB devices")?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .ok_or_else(|| anyhow!("Maschine MK3 not found on USB"))?;
        let dev = info.open().context(
            "opening the MK3 usbfs node -- install udev/98-maschine-mk3.rules and replug",
        )?;
        let iface = dev
            .claim_interface(BD_INTERFACE)
            .context("claiming USB interface 5 (display)")?;
        Ok(Self {
            iface,
            // header + rect + two commands + a full frame + trailer
            buf: Vec::with_capacity(32 + PIXELS * 2),
        })
    }

    /// Push the dirty rows of `frame` to display `index` (0 = left, 1 = right).
    ///
    /// Clears the dirty range on success; a partial write leaves it set so the
    /// next flush retries.
    pub fn flush(&mut self, index: u8, frame: &mut Frame) -> Result<()> {
        if frame.is_clean() {
            return Ok(());
        }
        let y0 = frame.dirty_lo;
        let rows = frame.dirty_hi - y0;
        self.blit(index, 0, y0, W, rows, &frame.px[y0 * W..(y0 + rows) * W])?;
        frame.dirty_lo = H;
        frame.dirty_hi = 0;
        Ok(())
    }

    /// Send an arbitrary rectangle of RGB565 pixels to one display.
    pub fn blit(
        &mut self,
        index: u8,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        px: &[u16],
    ) -> Result<()> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        if px.len() < w * h {
            return Err(anyhow!("blit: {} pixels supplied for {w}x{h}", px.len()));
        }
        // The length field counts 32-bit words, so an odd pixel count would
        // truncate. Every caller here uses full 480-pixel rows.
        if (w * h) % 2 != 0 {
            return Err(anyhow!("blit: pixel count {} must be even", w * h));
        }

        let b = &mut self.buf;
        b.clear();
        b.extend_from_slice(&[0x84, 0x00, index, 0x60, 0x00, 0x00, 0x00, 0x00]);
        b.extend_from_slice(&(x as u16).to_be_bytes());
        b.extend_from_slice(&(y as u16).to_be_bytes());
        b.extend_from_slice(&(w as u16).to_be_bytes());
        b.extend_from_slice(&(h as u16).to_be_bytes());
        b.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
        b.extend_from_slice(&((w * h / 2) as u32).to_be_bytes());
        for p in &px[..w * h] {
            b.extend_from_slice(&p.to_be_bytes());
        }
        b.extend_from_slice(&[
            0x02, 0x00, 0x00, 0x00, // end of pixel data
            0x03, 0x00, 0x00, 0x00, // blit
            0x40, 0x00, 0x00, 0x00, // end of frame
        ]);

        let payload = std::mem::take(&mut self.buf);
        let done = futures_lite::future::block_on(self.iface.bulk_out(BD_ENDPOINT, payload));
        // Reclaim the allocation regardless of outcome so steady-state is alloc-free.
        self.buf = done.data.reuse();
        match done.status {
            Ok(()) => Ok(()),
            Err(TransferError::Cancelled) => Ok(()),
            Err(e) => Err(anyhow!("display bulk transfer failed: {e}")),
        }
    }
}
