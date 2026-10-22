//! Logitech G502 HERO (046d:c08b) lighting over HID++ 2.0, feature 0x8070 (Color LED Effects).
//!
//! Protocol reference: OpenRGB's LogitechProtocolCommon and libratbag. Long reports are
//! `[0x11, 0xff, feature_index, function << 4 | sw_id, params[16]]`. Error replies carry
//! feature index 0xff. With software control enabled (function 0x8) the mouse holds whatever
//! "fixed" effect color we last set per zone; disabling it restores the onboard profile.

use std::time::{Duration, Instant};

use crate::hidraw::{self, HidDevice};
use crate::{Error, Result, Rgb};

pub const VID: u16 = 0x046d;
pub const PID: u16 = 0xc08b;

const LONG: u8 = 0x11;
const DEVICE_INDEX: u8 = 0xff;
const SW_ID: u8 = 0x0a;
const TIMEOUT: Duration = Duration::from_millis(300);

const FEATURE_ROOT: u8 = 0x00;
const PAGE_COLOR_LED_EFFECTS: u16 = 0x8070;

const FN_GET_INFO: u8 = 0x0;
const FN_GET_ZONE_INFO: u8 = 0x1;
const FN_GET_EFFECT_INFO: u8 = 0x2;
const FN_SET_ZONE_EFFECT: u8 = 0x3;
const FN_SET_SW_CONTROL: u8 = 0x8;

/// Effect id for "fixed color" in the 0x8070 effect table.
const EFFECT_FIXED: u16 = 0x0001;

#[derive(Debug, Clone)]
pub struct Zone {
    pub index: u8,
    /// 0x8070 zone location: 1 = primary (DPI strip), 2 = logo.
    pub location: u16,
    fixed_effect: u8,
}

impl Zone {
    pub fn name(&self) -> &'static str {
        match self.location {
            1 => "primary",
            2 => "logo",
            _ => "zone",
        }
    }
}

pub struct G502 {
    dev: HidDevice,
    feature: u8,
    pub zones: Vec<Zone>,
    sw_control: bool,
    last: Vec<Option<Rgb>>,
}

impl G502 {
    pub fn open() -> Result<Self> {
        let info = hidraw::find(VID, PID, Some(1)).ok_or(Error::NotFound)?;
        let dev = HidDevice::open(info)?;
        dev.drain();
        let mut g = Self { dev, feature: 0, zones: Vec::new(), sw_control: false, last: Vec::new() };
        let page = PAGE_COLOR_LED_EFFECTS.to_be_bytes();
        g.feature = g.request(FEATURE_ROOT, 0x0, &page)?[0];
        if g.feature == 0 {
            return Err(Error::Protocol("mouse does not expose HID++ feature 0x8070".into()));
        }
        let zone_count = g.request(g.feature, FN_GET_INFO, &[])?[0];
        for zi in 0..zone_count {
            let z = g.request(g.feature, FN_GET_ZONE_INFO, &[zi])?;
            let location = u16::from_be_bytes([z[1], z[2]]);
            let effect_count = z[3];
            let mut fixed = None;
            for ei in 0..effect_count {
                let e = g.request(g.feature, FN_GET_EFFECT_INFO, &[zi, ei])?;
                if u16::from_be_bytes([e[2], e[3]]) == EFFECT_FIXED {
                    fixed = Some(ei);
                    break;
                }
            }
            match fixed {
                Some(fixed_effect) => g.zones.push(Zone { index: zi, location, fixed_effect }),
                None => log::warn!("G502 zone {zi} has no fixed-color effect"),
            }
        }
        g.last = vec![None; g.zones.len()];
        Ok(g)
    }

    pub fn serial(&self) -> &str {
        &self.dev.info.serial
    }

    fn request(&self, feature: u8, function: u8, params: &[u8]) -> Result<[u8; 16]> {
        let mut buf = [0u8; 20];
        buf[0] = LONG;
        buf[1] = DEVICE_INDEX;
        buf[2] = feature;
        buf[3] = (function << 4) | SW_ID;
        buf[4..4 + params.len()].copy_from_slice(params);
        self.dev.write(&buf)?;

        let deadline = Instant::now() + TIMEOUT;
        let mut r = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let n = self.dev.read_timeout(&mut r, left.as_millis() as i32)?;
            if n < 20 || (r[0] != LONG && r[0] != 0x10) || r[1] != DEVICE_INDEX {
                continue; // button/key reports share this interface
            }
            if r[2] == 0xff && r[3] == feature && r[4] == buf[3] {
                return Err(Error::Protocol(format!("HID++ error {:#04x} (feature {feature:#04x} fn {function})", r[5])));
            }
            if r[2] == feature && r[3] == buf[3] {
                let mut out = [0u8; 16];
                out.copy_from_slice(&r[4..20]);
                return Ok(out);
            }
        }
    }

    /// Takes over the LEDs from the onboard profile (or hands them back).
    pub fn set_software_control(&mut self, on: bool) -> Result<()> {
        let v = on as u8;
        self.request(self.feature, FN_SET_SW_CONTROL, &[v, v])?;
        self.sw_control = on;
        self.last.iter_mut().for_each(|l| *l = None);
        Ok(())
    }

    /// One color per zone, in `zones` order. Unchanged zones are skipped.
    pub fn set_colors(&mut self, colors: &[Rgb]) -> Result<()> {
        for (i, &c) in colors.iter().enumerate().take(self.zones.len()) {
            if self.last[i] == Some(c) {
                continue;
            }
            let z = &self.zones[i];
            let mut p = [0u8; 13];
            p[0] = z.index;
            p[1] = z.fixed_effect;
            p[2..5].copy_from_slice(&[c.0, c.1, c.2]);
            // p[12] = 0: don't persist to onboard memory.
            self.request(self.feature, FN_SET_ZONE_EFFECT, &p)?;
            self.last[i] = Some(c);
        }
        Ok(())
    }

    /// Forces the next `set_colors` to resend every zone.
    pub fn invalidate(&mut self) {
        self.last.iter_mut().for_each(|l| *l = None);
    }
}

impl Drop for G502 {
    fn drop(&mut self) {
        if self.sw_control {
            let _ = self.set_software_control(false);
        }
    }
}
