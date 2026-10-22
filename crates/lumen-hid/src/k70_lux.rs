//! Corsair K70 LUX (1b1c:1b36): single-color (red) per-key backlight, legacy Corsair protocol.
//!
//! Protocol reference: OpenRGB's CorsairPeripheralController and OpenLinkHub's k70lux driver.
//! Only the lighting-control property (0x05) is switched to software mode; the special-function
//! property (0x04), which reroutes key events, is left alone so typing is unaffected.
//! The first switch to software mode after power-on can make the keyboard reset and re-enumerate;
//! callers should treat a write error as a disconnect and reopen.

use crate::hidraw::{self, HidDevice};
use crate::{Error, Result};

pub const VID: u16 = 0x1b1c;
pub const PID: u16 = 0x1b36;
/// Brightness slots addressed by the firmware (not all are populated keys).
pub const LEDS: usize = 168;

const PACKET: usize = 65;
const CMD_WRITE: u8 = 0x07;
const CMD_STREAM: u8 = 0x7f;
const PROP_LIGHTING_CONTROL: u8 = 0x05;
const PROP_SUBMIT_COLOR_24: u8 = 0x28;

pub struct K70Lux {
    dev: HidDevice,
    software: bool,
}

impl K70Lux {
    pub fn open() -> Result<Self> {
        let info = hidraw::find(VID, PID, Some(1)).ok_or(Error::NotFound)?;
        Ok(Self { dev: HidDevice::open(info)?, software: false })
    }

    pub fn serial(&self) -> &str {
        &self.dev.info.serial
    }

    fn send(&self, bytes: &[u8]) -> Result<()> {
        let mut buf = [0u8; PACKET];
        buf[1..1 + bytes.len()].copy_from_slice(bytes);
        self.dev.write(&buf)?;
        Ok(())
    }

    pub fn set_software_mode(&mut self, on: bool) -> Result<()> {
        let mode = if on { 0x02 } else { 0x01 };
        self.send(&[CMD_WRITE, PROP_LIGHTING_CONTROL, mode, 0x00, 0x03])?;
        self.software = on;
        Ok(())
    }

    /// One brightness byte per LED slot (`LEDS` long; shorter input is zero-padded).
    pub fn write_brightness(&self, levels: &[u8]) -> Result<()> {
        let mut data = [0u8; LEDS];
        let n = levels.len().min(LEDS);
        data[..n].copy_from_slice(&levels[..n]);
        for (i, chunk) in data.chunks(60).enumerate() {
            let mut pkt = vec![CMD_STREAM, i as u8 + 1, chunk.len() as u8, 0x00];
            pkt.extend_from_slice(chunk);
            self.send(&pkt)?;
        }
        // Submit channel 1 (red), 3 packets, final.
        self.send(&[CMD_WRITE, PROP_SUBMIT_COLOR_24, 0x01, 0x03, 0x02])
    }
}

impl Drop for K70Lux {
    fn drop(&mut self) {
        if self.software {
            let _ = self.set_software_mode(false);
        }
    }
}
