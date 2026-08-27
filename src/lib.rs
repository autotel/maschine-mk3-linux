//! Userspace driver for the Native Instruments Maschine MK3 on Linux.
//!
//! The device exposes seven USB interfaces. This driver uses two of them:
//!
//! * interface 4 -- HID (`/dev/hidraw*`): pads, buttons, knobs, encoder, LEDs.
//! * interface 5 -- vendor specific "Maschine MK3 BD": bulk pixel data for the
//!   two 480x272 displays. The kernel binds no driver to it, so it can be
//!   claimed from userspace without unbinding anything.
//!
//! Audio (interfaces 0-2) and the DIN MIDI jacks (interface 3) are already
//! handled by `snd-usb-audio`; the driver deliberately leaves them alone.

pub mod config;
pub mod config_default;
pub mod device;
pub mod display;
pub mod engine;
pub mod hid;
pub mod leds;
pub mod midi;
pub mod rt;
pub mod gui;
pub mod ui;

/// USB vendor id of Native Instruments.
pub const VID: u16 = 0x17cc;
/// USB product id of the Maschine MK3.
pub const PID: u16 = 0x1600;
