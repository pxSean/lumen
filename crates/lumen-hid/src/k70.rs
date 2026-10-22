//! Corsair K70 LUX (1b1c:1b36, red-only backlight) and K70 LUX RGB (1b1c:1b33), legacy
//! Corsair peripheral protocol.
//!
//! Protocol reference: OpenRGB's CorsairPeripheralController and OpenLinkHub's k70lux driver.
//! Only the lighting-control property (0x05) is switched to software mode; the special-function
//! property (0x04), which reroutes key events, is left alone so typing is unaffected.
//! The first switch to software mode after power-on can make the keyboard reset and re-enumerate;
//! callers should treat a write error as a disconnect and reopen.
//!
//! Colors are streamed per channel: stream packets (0x7f) carry one byte per hardware LED slot,
//! then a submit (0x07 0x28 channel packets finish) latches that channel. The red-only LUX takes
//! just the red channel.

use crate::hidraw::{self, HidDevice};
use crate::{Error, Result, Rgb};

pub const VID: u16 = 0x1b1c;
pub const PID_LUX: u16 = 0x1b36;
pub const PID_LUX_RGB: u16 = 0x1b33;

const PACKET: usize = 65;
const CMD_WRITE: u8 = 0x07;
const CMD_STREAM: u8 = 0x7f;
const PROP_LIGHTING_CONTROL: u8 = 0x05;
const PROP_SUBMIT_COLOR_24: u8 = 0x28;

/// Brightness slots addressed by the red-only LUX firmware (not all are populated keys).
const LUX_SLOTS: usize = 168;
/// Per-channel buffer for the LUX RGB; the highest populated slot is 141.
const RGB_SLOTS: usize = 144;

/// LUX RGB hardware slot for each LED, ordered left to right (column-major, top to bottom),
/// derived from OpenRGB's `keys_k70_mk2` and `matrix_map_k70_lux_rgb`. The last three (logo
/// halves and profile key) have no grid position.
const RGB_SLOT_ORDER: [u8; 116] = [
    0, 1, 2, 3, 4, 5, 13, 16, 12, 25, 14, 15, 17, 24, 37, 26, 27, 28, 29, 36, 49, 38, 39, 40, 48, 61,
    50, 51, 52, 62, 63, 64, 60, 73, 74, 75, 76, 53, 72, 85, 86, 87, 88, 84, 97, 98, 99, 100, 96, 109,
    110, 111, 112, 121, 124, 89, 108, 133, 122, 123, 120, 7, 134, 135, 136, 101, 132, 31, 90, 126, 79,
    113, 6, 102, 114, 91, 137, 18, 54, 43, 115, 8, 30, 66, 55, 103, 127, 42, 78, 67, 139, 32, 80, 9,
    57, 93, 129, 20, 44, 92, 21, 69, 105, 56, 104, 33, 81, 117, 141, 68, 116, 128, 140, 47, 59, 125,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    Lux,
    LuxRgb,
}

impl Model {
    pub fn name(self) -> &'static str {
        match self {
            Model::Lux => "Corsair K70 LUX",
            Model::LuxRgb => "Corsair K70 LUX RGB",
        }
    }

    /// Number of colors [`K70::write_colors`] expects.
    pub fn leds(self) -> usize {
        match self {
            Model::Lux => LUX_SLOTS,
            Model::LuxRgb => RGB_SLOT_ORDER.len(),
        }
    }
}

pub struct K70 {
    dev: HidDevice,
    pub model: Model,
    software: bool,
}

impl K70 {
    /// Opens whichever supported K70 is connected (the RGB model first).
    pub fn open() -> Result<Self> {
        for (pid, model) in [(PID_LUX_RGB, Model::LuxRgb), (PID_LUX, Model::Lux)] {
            if let Some(info) = hidraw::find(VID, pid, Some(1)) {
                return Ok(Self { dev: HidDevice::open(info)?, model, software: false });
            }
        }
        Err(Error::NotFound)
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

    /// `colors` has [`Model::leds`] entries, ordered left to right on the RGB model.
    /// The red-only LUX shows each color's perceived brightness.
    pub fn write_colors(&self, colors: &[Rgb]) -> Result<()> {
        match self.model {
            Model::Lux => {
                let levels: Vec<u8> = colors
                    .iter()
                    .map(|c| (0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32).round() as u8)
                    .collect();
                let mut data = [0u8; LUX_SLOTS];
                let n = levels.len().min(LUX_SLOTS);
                data[..n].copy_from_slice(&levels[..n]);
                self.send_channel(1, &data, true)
            }
            Model::LuxRgb => {
                let mut channels = [[0u8; RGB_SLOTS]; 3];
                for (&slot, c) in RGB_SLOT_ORDER.iter().zip(colors) {
                    let slot = slot as usize;
                    channels[0][slot] = c.0;
                    channels[1][slot] = c.1;
                    channels[2][slot] = c.2;
                }
                for (i, data) in channels.iter().enumerate() {
                    self.send_channel(i as u8 + 1, data, i == 2)?;
                }
                Ok(())
            }
        }
    }

    /// Streams one channel's bytes in 60-byte packets, then submits it.
    /// `last` marks the final channel of the frame so the keyboard applies it.
    fn send_channel(&self, channel: u8, data: &[u8], last: bool) -> Result<()> {
        let packets = data.chunks(60).count() as u8;
        for (i, chunk) in data.chunks(60).enumerate() {
            let mut pkt = vec![CMD_STREAM, i as u8 + 1, chunk.len() as u8, 0x00];
            pkt.extend_from_slice(chunk);
            self.send(&pkt)?;
        }
        self.send(&[CMD_WRITE, PROP_SUBMIT_COLOR_24, channel, packets, if last { 0x02 } else { 0x01 }])
    }
}

impl Drop for K70 {
    fn drop(&mut self) {
        if self.software {
            let _ = self.set_software_mode(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_slot_order_is_a_permutation_within_bounds() {
        let mut seen = [false; RGB_SLOTS];
        for &s in &RGB_SLOT_ORDER {
            assert!((s as usize) < RGB_SLOTS);
            assert!(!seen[s as usize], "slot {s} listed twice");
            seen[s as usize] = true;
        }
    }
}
