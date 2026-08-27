//! Locating and opening the Maschine MK3's HID interface.
//!
//! `hidraw` is used directly rather than through `hidapi`: the node is a plain
//! character device, reads return one report each, and dropping the C library
//! removes a copy and a build dependency from the input path.

use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::{PID, VID};

/// An open handle on the MK3 HID interface.
pub struct HidDev {
    file: File,
    path: PathBuf,
}

impl HidDev {
    /// Open the first `hidraw` node belonging to a Maschine MK3.
    pub fn open() -> Result<Self> {
        let path = find_hidraw()?;
        Self::open_path(&path)
    }

    /// Open a specific `hidraw` node.
    pub fn open_path(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| {
                format!(
                    "opening {}: install udev/98-maschine-mk3.rules and replug the device",
                    path.display()
                )
            })?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Path of the underlying node, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Block until one report arrives; returns the slice of `buf` that was filled.
    ///
    /// The first byte is the report id, because the MK3 uses numbered reports.
    pub fn read_report<'a>(&mut self, buf: &'a mut [u8]) -> Result<&'a [u8]> {
        let n = self.file.read(buf).context("reading HID report")?;
        Ok(&buf[..n])
    }

    /// Read with a timeout, so a shutdown request is noticed on an idle device.
    ///
    /// Returns `Ok(None)` when the timeout expires with no report pending.
    pub fn read_report_timeout<'a>(
        &mut self,
        buf: &'a mut [u8],
        timeout_ms: i32,
    ) -> Result<Option<&'a [u8]>> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd owned by this call.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(err).context("poll on hidraw");
        }
        if rc == 0 {
            return Ok(None);
        }
        let n = self.file.read(buf).context("reading HID report")?;
        Ok(Some(&buf[..n]))
    }

    /// Write an output report. `data[0]` must be the report id.
    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data).context("writing HID report")?;
        Ok(())
    }

    /// Duplicate the handle so the LED thread can write while the input thread reads.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.try_clone().context("dup hidraw fd")?,
            path: self.path.clone(),
        })
    }
}

/// Scan `/sys/class/hidraw` for a node whose parent USB device is the MK3.
pub fn find_hidraw() -> Result<PathBuf> {
    let want = format!("{VID:04X}:{PID:04X}");
    let dir = std::fs::read_dir("/sys/class/hidraw").context("listing /sys/class/hidraw")?;
    for entry in dir.flatten() {
        let uevent = entry.path().join("device/uevent");
        let Ok(text) = std::fs::read_to_string(&uevent) else {
            continue;
        };
        // HID_ID looks like "0003:000017CC:00001600"; match the tail of each field.
        let matches = text.lines().any(|line| {
            line.strip_prefix("HID_ID=").is_some_and(|id| {
                let mut parts = id.split(':').skip(1);
                match (parts.next(), parts.next()) {
                    (Some(v), Some(p)) => {
                        v.trim_start_matches('0').eq_ignore_ascii_case(
                            want.split(':').next().unwrap().trim_start_matches('0'),
                        ) && p.trim_start_matches('0').eq_ignore_ascii_case(
                            want.split(':').nth(1).unwrap().trim_start_matches('0'),
                        )
                    }
                    _ => false,
                }
            })
        });
        if matches {
            return Ok(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    Err(anyhow!(
        "no hidraw node for {want} -- is the Maschine MK3 plugged in?"
    ))
}

// ---------------------------------------------------------------------------
// Feature reports
// ---------------------------------------------------------------------------

const HID_IOC_MAGIC: u32 = b'H' as u32;

const fn hidioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    (((dir << 30) | ((size as u32) << 16) | (HID_IOC_MAGIC << 8) | nr) as libc::c_ulong)
        as libc::c_ulong
}

/// `_IOC_READ | _IOC_WRITE`
const RW: u32 = 3;

impl HidDev {
    /// Issue `HIDIOCSFEATURE`. `data[0]` must be the report id.
    pub fn set_feature(&mut self, data: &[u8]) -> Result<()> {
        let req = hidioc(RW, 0x06, data.len());
        // SAFETY: ioctl size encoded in `req` matches `data.len()`; the kernel
        // only reads from the buffer for SFEATURE.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), req, data.as_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("HIDIOCSFEATURE report 0x{:02x}", data[0]));
        }
        Ok(())
    }

    /// Issue `HIDIOCGFEATURE`. `buf[0]` must be the report id on entry;
    /// returns the slice the kernel filled.
    pub fn get_feature<'a>(&mut self, buf: &'a mut [u8]) -> Result<&'a [u8]> {
        let rid = buf[0];
        let req = hidioc(RW, 0x07, buf.len());
        // SAFETY: as above, but the kernel writes into the buffer.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), req, buf.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("HIDIOCGFEATURE report 0x{rid:02x}"));
        }
        let n = rc as usize;
        Ok(&buf[..n.min(buf.len())])
    }

    /// Set brightness and contrast (both 0..=100) of one display.
    ///
    /// Feature reports `0xf8` and `0xf9` describe the left and right panel.
    /// They read back as `w:u16 h:u16 bpp:u8 ?:u8 ?:u8 brightness:u8
    /// contrast:u8 ?:u8`, so the geometry fields are read first and echoed
    /// back unchanged.
    pub fn set_display_backlight(&mut self, index: u8, brightness: u8, contrast: u8) -> Result<()> {
        let rid = if index == 0 { 0xf8 } else { 0xf9 };
        let mut buf = [0u8; 11];
        buf[0] = rid;
        let cur = self.get_feature(&mut buf)?;
        let mut out = [0u8; 11];
        out[..cur.len().min(11)].copy_from_slice(&cur[..cur.len().min(11)]);
        out[0] = rid;
        out[8] = brightness.min(100);
        out[9] = contrast.min(100);
        self.set_feature(&out)
    }
}

impl std::os::unix::io::AsFd for HidDev {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        std::os::unix::io::AsFd::as_fd(&self.file)
    }
}
