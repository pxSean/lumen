//! Corsair iCUE LINK System Hub (1b1c:0c3f).
//!
//! Protocol reference: OpenLinkHub (GPL-3.0) and OpenRGB's CorsairICueLinkController (GPL-2.0+).
//! Every transfer is a 513-byte output report `[0, 0x00, 0x01, cmd.., data..]` answered by one
//! 512-byte input report where byte 3 is a status code and bytes 4..6 echo the data type.
//! Endpoints (0x36 devices, 0x17 speeds, 0x21 temps, 0x18 set speed, 0x22 colors) are opened,
//! read or written, then closed.

use std::time::Duration;

use crate::hidraw::{self, HidDevice};
use crate::{Error, Result, Rgb};

pub const VID: u16 = 0x1b1c;
pub const PID: u16 = 0x0c3f;

/// Pumps below this duty cycle risk the CPU; the driver never sends less.
pub const PUMP_MIN_PERCENT: u8 = 50;
/// Extra LEDs on an AIO pump head when the LCD cap is fitted.
const LCD_CAP_LEDS: u16 = 24;

const WRITE_LEN: usize = 513;
const READ_LEN: usize = 512;
const MAX_CHUNK: usize = 508;
const TIMEOUT_MS: i32 = 500;

const CMD_OPEN_ENDPOINT: &[u8] = &[0x0d, 0x01];
const CMD_OPEN_COLOR_ENDPOINT: &[u8] = &[0x0d, 0x00];
const CMD_CLOSE_ENDPOINT: &[u8] = &[0x05, 0x01, 0x01];
const CMD_GET_FIRMWARE: &[u8] = &[0x02, 0x13];
const CMD_SOFTWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x02];
const CMD_HARDWARE_MODE: &[u8] = &[0x01, 0x03, 0x00, 0x01];
const CMD_WRITE: &[u8] = &[0x06, 0x01];
const CMD_WRITE_COLOR: &[u8] = &[0x06, 0x00];
const CMD_WRITE_COLOR_NEXT: &[u8] = &[0x07, 0x00];
const CMD_READ: &[u8] = &[0x08, 0x01];

const EP_DEVICES: &[u8] = &[0x36];
const EP_TEMPS: &[u8] = &[0x21];
const EP_SPEEDS: &[u8] = &[0x17];
const EP_SET_SPEED: &[u8] = &[0x18];
const EP_SET_COLOR: &[u8] = &[0x22];

const DT_DEVICES: &[u8] = &[0x21, 0x00];
const DT_TEMPS: &[u8] = &[0x10, 0x00];
const DT_SPEEDS: &[u8] = &[0x25, 0x00];
const DT_SET_SPEED: &[u8] = &[0x07, 0x00];
const DT_SET_COLOR: &[u8] = &[0x12, 0x00];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fan,
    Aio,
    Pump,
    LcdCap,
    Other,
}

#[derive(Debug, Clone)]
pub struct LinkDevice {
    /// 1-based channel id; also the index into speed/temperature reports.
    pub channel: u8,
    pub type_id: u8,
    pub model: u8,
    pub serial: String,
    pub name: String,
    pub kind: Kind,
    pub leds: u16,
    pub has_speed: bool,
}

struct Meta {
    type_id: u8,
    model: Option<u8>,
    name: &'static str,
    leds: u16,
    kind: Kind,
    has_speed: bool,
}

const fn m(type_id: u8, model: Option<u8>, name: &'static str, leds: u16, kind: Kind, has_speed: bool) -> Meta {
    Meta { type_id, model, name, leds, kind, has_speed }
}

// type/model table from OpenLinkHub's database/external/lsh.json
const KNOWN: &[Meta] = &[
    m(0x01, None, "iCUE LINK QX RGB", 34, Kind::Fan, true),
    m(0x02, None, "iCUE LINK LX RGB", 18, Kind::Fan, true),
    m(0x03, None, "iCUE LINK RX RGB MAX", 8, Kind::Fan, true),
    m(0x04, None, "iCUE LINK RX MAX", 0, Kind::Fan, true),
    m(0x0f, None, "iCUE LINK RX RGB", 8, Kind::Fan, true),
    m(0x13, None, "iCUE LINK RX", 0, Kind::Fan, true),
    m(0x06, None, "iCUE LINK LCD Pump Cap", 0, Kind::LcdCap, false),
    m(0x07, Some(0), "iCUE LINK H100i", 20, Kind::Aio, true),
    m(0x07, Some(1), "iCUE LINK H115i", 20, Kind::Aio, true),
    m(0x07, Some(2), "iCUE LINK H150i", 20, Kind::Aio, true),
    m(0x07, Some(3), "iCUE LINK H170i", 20, Kind::Aio, true),
    m(0x07, Some(4), "iCUE LINK H100i", 20, Kind::Aio, true),
    m(0x07, Some(5), "iCUE LINK H150i", 20, Kind::Aio, true),
    m(0x11, Some(0), "iCUE LINK TITAN 240", 20, Kind::Aio, true),
    m(0x11, Some(1), "iCUE LINK TITAN 280", 20, Kind::Aio, true),
    m(0x11, Some(2), "iCUE LINK TITAN 360", 20, Kind::Aio, true),
    m(0x11, Some(3), "iCUE LINK TITAN 420", 20, Kind::Aio, true),
    m(0x11, Some(4), "iCUE LINK TITAN 240", 20, Kind::Aio, true),
    m(0x11, Some(5), "iCUE LINK TITAN 360", 20, Kind::Aio, true),
    m(0x0c, None, "iCUE LINK XD5 ELITE", 22, Kind::Pump, true),
    m(0x19, None, "iCUE LINK XD6 ELITE", 22, Kind::Pump, true),
    m(0x09, None, "iCUE LINK XC7 ELITE", 24, Kind::Other, false),
    m(0x0a, None, "iCUE LINK XG3 HYBRID", 22, Kind::Other, true),
    m(0x0d, None, "iCUE LINK XG7 RGB", 16, Kind::Other, false),
    m(0x10, None, "VRM Cooler Module", 0, Kind::Fan, true),
];

fn lookup(type_id: u8, model: u8) -> Option<&'static Meta> {
    KNOWN
        .iter()
        .find(|k| k.type_id == type_id && k.model.is_none_or(|km| km == model))
}

pub struct LinkHub {
    dev: HidDevice,
    pub firmware: String,
    pub devices: Vec<LinkDevice>,
    software_mode: bool,
}

impl LinkHub {
    pub fn open() -> Result<Self> {
        let info = hidraw::find(VID, PID, Some(0)).ok_or(Error::NotFound)?;
        let dev = HidDevice::open(info)?;
        dev.drain();
        let mut hub = Self {
            dev,
            firmware: String::new(),
            devices: Vec::new(),
            software_mode: false,
        };
        hub.firmware = hub.read_firmware()?;
        Ok(hub)
    }

    pub fn serial(&self) -> &str {
        &self.dev.info.serial
    }

    pub fn has_lcd_cap(&self) -> bool {
        self.devices.iter().any(|d| d.kind == Kind::LcdCap)
    }

    /// Takes control from the hub's onboard (hardware) mode and enumerates attached devices.
    pub fn enter_software_mode(&mut self) -> Result<()> {
        self.transfer(CMD_SOFTWARE_MODE, &[])?;
        self.software_mode = true;
        self.devices = self.enumerate()?;
        Ok(())
    }

    /// Returns the hub to its onboard fan curve and lighting.
    pub fn enter_hardware_mode(&mut self) -> Result<()> {
        self.transfer(CMD_HARDWARE_MODE, &[])?;
        self.software_mode = false;
        Ok(())
    }

    fn read_firmware(&self) -> Result<String> {
        let r = self.transfer(CMD_GET_FIRMWARE, &[])?;
        Ok(format!("{}.{}.{}", r[4], r[5], u16::from_le_bytes([r[6], r[7]])))
    }

    fn transfer(&self, cmd: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let mut w = [0u8; WRITE_LEN];
        w[2] = 0x01;
        w[3..3 + cmd.len()].copy_from_slice(cmd);
        w[3 + cmd.len()..3 + cmd.len() + data.len()].copy_from_slice(data);
        self.dev.write(&w)?;
        let mut r = vec![0u8; READ_LEN];
        match self.dev.read_timeout(&mut r, TIMEOUT_MS)? {
            0 => Err(Error::Timeout),
            _ => Ok(r),
        }
    }

    fn read_endpoint(&self, endpoint: &[u8], data_type: &[u8]) -> Result<Vec<u8>> {
        self.transfer(CMD_CLOSE_ENDPOINT, endpoint)?;
        self.transfer(CMD_OPEN_ENDPOINT, endpoint)?;
        let mut buf = self.transfer(CMD_READ, endpoint)?;
        if &buf[4..6] == data_type {
            // Larger payloads (device lists) continue in a second report.
            let next = self.transfer(CMD_READ, endpoint)?;
            buf.extend_from_slice(&next[4..]);
        }
        self.transfer(CMD_CLOSE_ENDPOINT, endpoint)?;
        if &buf[4..6] != data_type {
            return Err(Error::Protocol(format!(
                "endpoint {:02x?}: expected data type {:02x?}, got {:02x?}",
                endpoint,
                data_type,
                &buf[4..6]
            )));
        }
        Ok(buf)
    }

    fn write_endpoint(&self, endpoint: &[u8], data_type: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let payload = frame(data_type, data);
        self.transfer(CMD_CLOSE_ENDPOINT, endpoint)?;
        self.transfer(CMD_OPEN_ENDPOINT, endpoint)?;
        let resp = self.transfer(CMD_WRITE, &payload)?;
        self.transfer(CMD_CLOSE_ENDPOINT, endpoint)?;
        Ok(resp)
    }

    fn enumerate(&self) -> Result<Vec<LinkDevice>> {
        let r = self.read_endpoint(EP_DEVICES, DT_DEVICES)?;
        let channels = r[6];
        let data = &r[7..];
        let mut pos = 0usize;
        let mut out = Vec::new();
        for channel in 1..=channels {
            let Some(meta) = data.get(pos..pos + 8) else { break };
            let id_len = meta[7] as usize;
            if id_len == 0 {
                pos += 8;
                continue;
            }
            let (type_id, model) = (meta[2], meta[3]);
            let serial = data
                .get(pos + 8..pos + 8 + id_len)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default();
            pos += 8 + id_len;

            let Some(k) = lookup(type_id, model) else {
                log::warn!("iCUE LINK channel {channel}: unknown device type {type_id:#04x} model {model:#04x}");
                continue;
            };
            out.push(LinkDevice {
                channel,
                type_id,
                model,
                serial,
                name: k.name.to_string(),
                kind: k.kind,
                leds: k.leds,
                has_speed: k.has_speed,
            });
        }
        // The LCD cap's ring LEDs are driven as part of the AIO pump head.
        if out.iter().any(|d| d.kind == Kind::LcdCap) {
            for d in out.iter_mut().filter(|d| d.kind == Kind::Aio) {
                d.leds += LCD_CAP_LEDS;
                d.name.push_str(" LCD");
            }
        }
        Ok(out)
    }

    /// RPM per channel id (index 0 is the hub itself). `None` where the sensor is absent.
    pub fn read_speeds(&self) -> Result<Vec<Option<u16>>> {
        let r = self.read_endpoint(EP_SPEEDS, DT_SPEEDS)?;
        Ok(parse_sensors(&r, |v| v as u16))
    }

    /// Temperature in °C per channel id. AIOs report coolant temperature.
    pub fn read_temps(&self) -> Result<Vec<Option<f32>>> {
        let r = self.read_endpoint(EP_TEMPS, DT_TEMPS)?;
        Ok(parse_sensors(&r, |v| v as i16 as f32 / 10.0))
    }

    /// Sets duty cycles (0-100%) by channel. Pumps are clamped to [`PUMP_MIN_PERCENT`].
    pub fn set_speeds(&self, speeds: &[(u8, u8)]) -> Result<()> {
        let mut speeds: Vec<(u8, u8)> = speeds
            .iter()
            .map(|&(ch, pct)| {
                let pumpish = self
                    .devices
                    .iter()
                    .any(|d| d.channel == ch && matches!(d.kind, Kind::Aio | Kind::Pump));
                let floor = if pumpish { PUMP_MIN_PERCENT } else { 0 };
                (ch, pct.clamp(floor, 100))
            })
            .collect();
        speeds.sort_by_key(|s| s.0);
        let mut buf = vec![speeds.len() as u8];
        for (ch, pct) in speeds {
            buf.extend_from_slice(&[ch, 0x00, pct, 0x00]);
        }
        for attempt in 0..5 {
            let resp = self.write_endpoint(EP_SET_SPEED, DT_SET_SPEED, &buf)?;
            if resp[3] == 0 {
                return Ok(());
            }
            log::debug!("set_speeds attempt {attempt} rejected with status {:#04x}", resp[3]);
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(Error::Protocol("hub rejected speed update".into()))
    }

    /// Total addressable LEDs, in the order [`write_colors`](Self::write_colors) expects.
    pub fn led_count(&self) -> usize {
        self.devices.iter().map(|d| d.leds as usize).sum()
    }

    /// Writes a full frame of colors: every device's LEDs, concatenated in channel order.
    pub fn write_colors(&self, colors: &[Rgb]) -> Result<()> {
        let rgb: Vec<u8> = colors.iter().flat_map(|c| [c.0, c.1, c.2]).collect();
        let payload = frame(DT_SET_COLOR, &rgb);
        self.transfer(CMD_CLOSE_ENDPOINT, EP_SET_COLOR)?;
        self.transfer(CMD_OPEN_COLOR_ENDPOINT, EP_SET_COLOR)?;
        for (i, chunk) in payload.chunks(MAX_CHUNK).enumerate() {
            let cmd = if i == 0 { CMD_WRITE_COLOR } else { CMD_WRITE_COLOR_NEXT };
            self.transfer(cmd, chunk)?;
        }
        self.transfer(CMD_CLOSE_ENDPOINT, EP_SET_COLOR)?;
        Ok(())
    }
}

impl Drop for LinkHub {
    fn drop(&mut self) {
        if self.software_mode {
            if let Err(e) = self.enter_hardware_mode() {
                log::error!("failed to return iCUE LINK hub to hardware mode: {e}");
            }
        }
    }
}

/// `[len_lo, len_hi, 0, 0, data_type.., data..]` where len counts data plus the 2-byte type.
fn frame(data_type: &[u8], data: &[u8]) -> Vec<u8> {
    let len = (data.len() + 2) as u16;
    let mut buf = Vec::with_capacity(4 + data_type.len() + data.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(data_type);
    buf.extend_from_slice(data);
    buf
}

/// Sensor reports: byte 6 is the count, then 3 bytes each: status (0 = present), u16 LE value.
fn parse_sensors<T>(r: &[u8], conv: impl Fn(u16) -> T) -> Vec<Option<T>> {
    let count = r[6] as usize;
    (0..count)
        .map(|i| {
            let s = r.get(7 + i * 3..10 + i * 3)?;
            (s[0] == 0).then(|| conv(u16::from_le_bytes([s[1], s[2]])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout() {
        assert_eq!(frame(DT_SET_SPEED, &[1, 2, 3]), vec![5, 0, 0, 0, 0x07, 0x00, 1, 2, 3]);
    }

    #[test]
    fn sensors_skip_absent() {
        let mut r = vec![0u8; 32];
        r[6] = 3;
        r[7..10].copy_from_slice(&[0x00, 0xe8, 0x03]); // 1000
        r[10..13].copy_from_slice(&[0x01, 0xff, 0xff]); // absent
        r[13..16].copy_from_slice(&[0x00, 0x2c, 0x01]); // 300
        assert_eq!(parse_sensors(&r, |v| v), vec![Some(1000), None, Some(300)]);
    }

    #[test]
    fn lookup_model_specific() {
        assert_eq!(lookup(0x07, 2).unwrap().name, "iCUE LINK H150i");
        assert_eq!(lookup(0x01, 9).unwrap().name, "iCUE LINK QX RGB");
        assert!(lookup(0x7f, 0).is_none());
    }
}
