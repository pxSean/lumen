//! Generic worker for lighting-only devices (keyboard, mouse, motherboard headers).

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_hid::Rgb;
use lumen_hid::g502::G502;
use lumen_hid::k70_lux::{self, K70Lux};
use lumen_ipc::{DeviceStatus, ZoneStatus};

use crate::effects::{self, Ctx};
use crate::state::{Shared, lighting_for};

const FRAME: Duration = Duration::from_millis(33);

pub struct ZoneInfo {
    /// Target suffix: the zone's lighting key is `<device id>:<key>`.
    pub key: String,
    pub name: String,
    pub kind: &'static str,
    pub leds: usize,
}

pub trait LightDevice: Sized {
    const ID: &'static str;
    const NAME: &'static str;
    /// Resend unchanged frames this often.
    const KEEPALIVE: Duration;

    fn open() -> lumen_hid::Result<Self>;
    fn zones(&self) -> Vec<ZoneInfo>;
    /// One color slice per zone, in `zones()` order.
    fn write(&mut self, frame: &[Vec<Rgb>]) -> lumen_hid::Result<()>;
}

pub fn run<D: LightDevice>(shared: Arc<Shared>) {
    let mut announced_missing = false;
    while shared.running() {
        let err = match session::<D>(&shared) {
            Ok(()) => break,
            Err(e) => e,
        };
        let missing = matches!(err, lumen_hid::Error::NotFound);
        if !missing || !announced_missing {
            log::warn!("{}: {err}", D::NAME);
        }
        announced_missing = missing;
        shared.update_device(DeviceStatus {
            id: D::ID.into(),
            name: D::NAME.into(),
            connected: false,
            error: Some(err.to_string()),
            zones: Vec::new(),
        });
        let end = Instant::now() + Duration::from_secs(3);
        while shared.running() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn session<D: LightDevice>(shared: &Shared) -> lumen_hid::Result<()> {
    let mut dev = D::open()?;
    let zones = dev.zones();
    log::info!("{} connected: {} zone(s)", D::NAME, zones.len());
    shared.update_device(DeviceStatus {
        id: D::ID.into(),
        name: D::NAME.into(),
        connected: true,
        error: None,
        zones: zones
            .iter()
            .map(|z| ZoneStatus {
                target: format!("{}:{}", D::ID, z.key),
                name: z.name.clone(),
                kind: z.kind.into(),
                leds: z.leds as u16,
                rpm: None,
                temp: None,
                duty: None,
            })
            .collect(),
    });

    let keys: Vec<[String; 2]> = zones.iter().map(|z| [format!("{}:{}", D::ID, z.key), D::ID.to_string()]).collect();
    let mut generation = 0;
    let mut effects = Vec::new();
    let mut last: Vec<Vec<Rgb>> = Vec::new();
    let mut last_write = Instant::now() - D::KEEPALIVE;

    while shared.running() {
        let start = Instant::now();
        if shared.generation() != generation {
            generation = shared.generation();
            let profile = shared.active_profile();
            effects = keys.iter().map(|k| lighting_for(&profile, &[&k[0], &k[1]])).collect();
        }
        let frame: Vec<Vec<Rgb>> = {
            let theme = shared.theme.read().unwrap();
            let ctx = Ctx {
                t: shared.started.elapsed().as_secs_f32(),
                theme: &theme,
                sensors: shared.runtime().sensors,
            };
            zones.iter().zip(&effects).map(|(z, e)| effects::render(e, z.leds, &ctx)).collect()
        };
        if frame != last || last_write.elapsed() >= D::KEEPALIVE {
            dev.write(&frame)?;
            last = frame;
            last_write = Instant::now();
        }
        if let Some(rest) = FRAME.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    Ok(())
}

impl LightDevice for K70Lux {
    const ID: &'static str = "k70";
    const NAME: &'static str = "Corsair K70 LUX";
    const KEEPALIVE: Duration = Duration::from_secs(5);

    fn open() -> lumen_hid::Result<Self> {
        let mut k = K70Lux::open()?;
        k.set_software_mode(true)?;
        Ok(k)
    }

    fn zones(&self) -> Vec<ZoneInfo> {
        vec![ZoneInfo { key: "keys".into(), name: "Keys (red backlight)".into(), kind: "keyboard", leds: k70_lux::LEDS }]
    }

    fn write(&mut self, frame: &[Vec<Rgb>]) -> lumen_hid::Result<()> {
        // Red-only LEDs: show each color's perceived brightness.
        let levels: Vec<u8> = frame[0]
            .iter()
            .map(|c| (0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32).round() as u8)
            .collect();
        self.write_brightness(&levels)
    }
}

impl LightDevice for G502 {
    const ID: &'static str = "g502";
    const NAME: &'static str = "Logitech G502 HERO";
    const KEEPALIVE: Duration = Duration::from_secs(10);

    fn open() -> lumen_hid::Result<Self> {
        let mut g = G502::open()?;
        g.set_software_control(true)?;
        Ok(g)
    }

    fn zones(&self) -> Vec<ZoneInfo> {
        self.zones
            .iter()
            .map(|z| ZoneInfo { key: z.name().into(), name: z.name().into(), kind: "mouse", leds: 1 })
            .collect()
    }

    fn write(&mut self, frame: &[Vec<Rgb>]) -> lumen_hid::Result<()> {
        let colors: Vec<Rgb> = frame.iter().map(|z| z.first().copied().unwrap_or_default()).collect();
        self.invalidate();
        self.set_colors(&colors)
    }
}
