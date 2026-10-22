//! iCUE LINK hub worker: lighting frames, sensor polling and fan control.
//!
//! Safety rules:
//! - Pumps never drop below `icue_link::PUMP_MIN_PERCENT` (enforced in the driver).
//! - Coolant at or above `CRITICAL_COOLANT` forces every channel to 100% until it falls below
//!   `CRITICAL_RELEASE`.
//! - A curve whose temperature source is unavailable evaluates to 100%.
//! - Dropping the hub (normal exit, error, or panic unwind) returns it to hardware mode, so its
//!   onboard fan curve takes over.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_hid::Rgb;
use lumen_hid::icue_link::{Kind, LinkDevice, LinkHub};
use lumen_ipc::{DeviceStatus, Effect, FanSpec, Profile, Sensors, ZoneStatus};

use crate::effects::{self, Ctx};
use crate::state::{Shared, fan_for, lighting_for};

const FRAME: Duration = Duration::from_millis(33);
const SENSOR_INTERVAL: Duration = Duration::from_secs(1);
/// Re-send unchanged colors/speeds this often so the hub never falls back to its defaults.
const KEEPALIVE: Duration = Duration::from_secs(2);
const CRITICAL_COOLANT: f32 = 55.0;
const CRITICAL_RELEASE: f32 = 50.0;
/// Max duty change per sensor tick.
const RAMP_UP: i16 = 20;
const RAMP_DOWN: i16 = 4;

pub fn run(shared: Arc<Shared>) {
    while shared.running() {
        if let Err(e) = session(&shared) {
            log::error!("iCUE LINK hub: {e:#}");
            shared.update_device(DeviceStatus {
                id: "hub".into(),
                name: "iCUE LINK System Hub".into(),
                connected: false,
                error: Some(format!("{e:#}")),
                zones: Vec::new(),
            });
            shared.runtime().sensors.coolant = None;
            sleep_while_running(&shared, Duration::from_secs(3));
        }
    }
}

fn sleep_while_running(shared: &Shared, d: Duration) {
    let end = Instant::now() + d;
    while shared.running() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Zone {
    start: usize,
    len: usize,
    keys: Vec<String>,
}

fn group_key(d: &LinkDevice) -> &'static str {
    match d.kind {
        Kind::Aio => "hub:aio",
        Kind::Pump => "hub:pump",
        Kind::Fan => "hub:fans",
        Kind::LcdCap | Kind::Other => "hub:other",
    }
}

fn fan_keys(d: &LinkDevice) -> [String; 2] {
    let group = if matches!(d.kind, Kind::Aio | Kind::Pump) { "pump" } else { "fans" };
    [format!("ch{}", d.channel), group.to_string()]
}

fn session(shared: &Shared) -> anyhow::Result<()> {
    let mut hub = LinkHub::open()?;
    hub.enter_software_mode()?;
    log::info!(
        "iCUE LINK hub {} firmware {}: {} devices, {} LEDs",
        hub.serial(),
        hub.firmware,
        hub.devices.len(),
        hub.led_count()
    );

    let mut zones = Vec::new();
    let mut start = 0;
    for d in hub.devices.iter().filter(|d| d.leds > 0) {
        zones.push(Zone {
            start,
            len: d.leds as usize,
            keys: vec![format!("hub:ch{}", d.channel), group_key(d).into(), "hub".into()],
        });
        start += d.leds as usize;
    }

    let mut fans = FanControl::default();
    let mut generation = 0;
    let mut profile = Profile::default();
    let mut effects: Vec<Effect> = Vec::new();
    let mut last_sensors = Instant::now() - SENSOR_INTERVAL;
    let mut last_colors: Vec<Rgb> = Vec::new();
    let mut last_color_write = Instant::now() - KEEPALIVE;

    while shared.running() {
        let frame_start = Instant::now();
        if shared.generation() != generation {
            generation = shared.generation();
            profile = shared.active_profile();
            effects = zones
                .iter()
                .map(|z| lighting_for(&profile, &z.keys.iter().map(String::as_str).collect::<Vec<_>>()))
                .collect();
            fans.force_send = true;
        }

        if last_sensors.elapsed() >= SENSOR_INTERVAL {
            last_sensors = Instant::now();
            poll_and_control(shared, &hub, &profile, &mut fans)?;
        }

        let sensors = shared.runtime().sensors;
        let colors = {
            let theme = shared.theme.read().unwrap();
            let ctx = Ctx { t: shared.started.elapsed().as_secs_f32(), theme: &theme, sensors };
            let mut colors = vec![Rgb::BLACK; hub.led_count()];
            for (z, e) in zones.iter().zip(&effects) {
                colors[z.start..z.start + z.len].copy_from_slice(&effects::render(e, z.len, &ctx));
            }
            colors
        };
        if colors != last_colors || last_color_write.elapsed() >= KEEPALIVE {
            hub.write_colors(&colors)?;
            last_colors = colors;
            last_color_write = Instant::now();
        }

        if let Some(rest) = FRAME.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    Ok(())
}

#[derive(Default)]
struct FanControl {
    duty: HashMap<u8, u8>,
    critical: bool,
    last_sent: Option<Instant>,
    force_send: bool,
}

fn poll_and_control(shared: &Shared, hub: &LinkHub, profile: &Profile, fans: &mut FanControl) -> anyhow::Result<()> {
    let rpm = hub.read_speeds()?;
    let temps = hub.read_temps()?;
    let coolant = hub
        .devices
        .iter()
        .find(|d| d.kind == Kind::Aio)
        .and_then(|d| temps.get(d.channel as usize).copied().flatten());

    let sensors = {
        let mut rt = shared.runtime();
        rt.sensors.coolant = coolant;
        rt.sensors
    };

    // Critical-temperature override with hysteresis.
    match coolant {
        Some(t) if t >= CRITICAL_COOLANT && !fans.critical => {
            log::warn!("coolant {t:.1}°C ≥ {CRITICAL_COOLANT}°C: forcing all fans and pump to 100%");
            fans.critical = true;
        }
        Some(t) if t < CRITICAL_RELEASE && fans.critical => {
            log::warn!("coolant {t:.1}°C: releasing critical override");
            fans.critical = false;
        }
        _ => {}
    }

    let defaults = Profile::default();
    let mut changed = false;
    let mut speeds = Vec::new();
    for d in hub.devices.iter().filter(|d| d.has_speed) {
        let keys = fan_keys(d);
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let spec = fan_for(profile, &keys)
            .or_else(|| fan_for(&defaults, &keys))
            .unwrap_or(FanSpec::Fixed { percent: 100 });
        let target = if fans.critical { 100 } else { spec.evaluate(source_temp(&spec, &sensors)) };
        let prev = fans.duty.get(&d.channel).copied();
        let next = match prev {
            Some(p) if !fans.critical => {
                let delta = (target as i16 - p as i16).clamp(-RAMP_DOWN, RAMP_UP);
                (p as i16 + delta) as u8
            }
            _ => target,
        };
        changed |= prev != Some(next);
        fans.duty.insert(d.channel, next);
        speeds.push((d.channel, next));
    }

    let due = fans.last_sent.is_none_or(|t| t.elapsed() >= KEEPALIVE);
    if !speeds.is_empty() && (changed || due || fans.force_send) {
        hub.set_speeds(&speeds)?;
        fans.last_sent = Some(Instant::now());
        fans.force_send = false;
    }

    let zones = hub
        .devices
        .iter()
        .filter(|d| d.kind != Kind::LcdCap)
        .map(|d| ZoneStatus {
            target: format!("hub:ch{}", d.channel),
            name: d.name.clone(),
            kind: format!("{:?}", d.kind).to_lowercase(),
            leds: d.leds,
            rpm: rpm.get(d.channel as usize).copied().flatten(),
            temp: temps.get(d.channel as usize).copied().flatten(),
            duty: fans.duty.get(&d.channel).copied(),
        })
        .collect();
    shared.runtime().critical = fans.critical;
    shared.update_device(DeviceStatus {
        id: "hub".into(),
        name: format!("iCUE LINK System Hub (fw {})", hub.firmware),
        connected: true,
        error: None,
        zones,
    });
    Ok(())
}

fn source_temp(spec: &FanSpec, s: &Sensors) -> Option<f32> {
    match spec {
        FanSpec::Fixed { .. } => None,
        FanSpec::Curve { source, .. } => effects::temp_for(*source, s),
    }
}
