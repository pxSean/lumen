//! Corsair iCUE LINK AIO LCD screen module (1b1c:0c4e): a 480×480 panel fed JPEG frames.
//!
//! Frames go out as 1024-byte output reports with report ID 0x02:
//! `[0x02, 0x05, 0x01, last, index, 0x00, len_lo, len_hi, jpeg[..1016]]`.
//! Backlight and rotation are 4-byte feature reports with report ID 0x03.
//! Reference implementations re-send a frame about once per second even when nothing changed.

use crate::hidraw::{self, HidDevice};
use crate::{Error, Result};

pub const VID: u16 = 0x1b1c;
pub const PID_AIO: u16 = 0x0c4e;
pub const WIDTH: u32 = 480;
pub const HEIGHT: u32 = 480;

const REPORT_LEN: usize = 1024;
const HEADER_LEN: usize = 8;
const CHUNK: usize = REPORT_LEN - HEADER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    R0 = 0,
    R90 = 1,
    R180 = 2,
    R270 = 3,
}

pub struct Lcd {
    dev: HidDevice,
}

impl Lcd {
    pub fn open() -> Result<Self> {
        let info = hidraw::find(VID, PID_AIO, Some(0)).ok_or(Error::NotFound)?;
        Ok(Self { dev: HidDevice::open(info)? })
    }

    pub fn serial(&self) -> &str {
        &self.dev.info.serial
    }

    /// Sends one complete JPEG frame (480×480, baseline).
    pub fn send_jpeg(&self, jpeg: &[u8]) -> Result<()> {
        let chunks: Vec<&[u8]> = jpeg.chunks(CHUNK).collect();
        let last = chunks.len().saturating_sub(1);
        let mut buf = [0u8; REPORT_LEN];
        for (i, chunk) in chunks.iter().enumerate() {
            buf.fill(0);
            buf[0] = 0x02;
            buf[1] = 0x05;
            buf[2] = 0x01;
            buf[3] = (i == last) as u8;
            buf[4] = i as u8;
            buf[6..8].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            buf[HEADER_LEN..HEADER_LEN + chunk.len()].copy_from_slice(chunk);
            self.dev.write(&buf)?;
        }
        Ok(())
    }

    /// Backlight brightness, 0-100. Zero turns the backlight off.
    pub fn set_brightness(&self, percent: u8) -> Result<()> {
        self.dev.send_feature(&[0x03, 0x0b, percent.min(100), 0x01])
    }

    pub fn set_rotation(&self, rotation: Rotation) -> Result<()> {
        self.dev.send_feature(&[0x03, 0x0c, rotation as u8, 0x01])
    }

    /// Sends a raw 4-byte feature report (report ID 0x03 first); for recovery tools.
    pub fn send_feature_raw(&self, report: &[u8; 4]) -> Result<()> {
        self.dev.send_feature(report)
    }

    /// Hands the panel back to its built-in firmware screen.
    pub fn release(&self) -> Result<()> {
        self.dev.send_feature(&[0x03, 0x1e, 0x01, 0x01])?;
        self.dev.send_feature(&[0x03, 0x1d, 0x00, 0x01])?;
        self.set_brightness(100)
    }
}
