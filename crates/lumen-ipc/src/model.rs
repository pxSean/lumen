//! Config model, stored as `~/.config/lumen/config.toml` and exchanged over IPC.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub active_profile: String,
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            active_profile: "default".into(),
            profiles: BTreeMap::from([("default".into(), Profile::default())]),
        }
    }
}

/// Lighting and fan targets are matched most-specific first:
/// `hub:ch3` → `hub:aio` / `hub:fans` → `hub` → `*`.
/// Fan targets are `pump`, `fans`, or `ch<N>` (hub channel), most-specific first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub lighting: BTreeMap<String, Effect>,
    #[serde(default)]
    pub fans: BTreeMap<String, FanSpec>,
    #[serde(default)]
    pub lcd: LcdSpec,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            lighting: BTreeMap::from([(
                "*".into(),
                Effect::Static { color: "theme".into() },
            )]),
            fans: BTreeMap::from([
                (
                    "pump".into(),
                    FanSpec::Curve {
                        source: TempSource::Coolant,
                        points: vec![(30.0, 60), (38.0, 75), (42.0, 90), (45.0, 100)],
                    },
                ),
                (
                    "fans".into(),
                    FanSpec::Curve {
                        source: TempSource::Coolant,
                        points: vec![(30.0, 30), (34.0, 40), (38.0, 55), (42.0, 75), (46.0, 100)],
                    },
                ),
            ]),
            lcd: LcdSpec::default(),
        }
    }
}

/// Colors are `#rrggbb`, `theme` (the Omarchy theme's keyboard.rgb), or `theme:<name>`
/// for any key in the theme's colors.toml (`theme:accent`, `theme:red`, ...).
pub type ColorRef = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    Off,
    Static {
        color: ColorRef,
    },
    Breathing {
        color: ColorRef,
        #[serde(default = "one")]
        speed: f32,
    },
    /// Every LED cycles through the hue wheel together.
    Rainbow {
        #[serde(default = "one")]
        speed: f32,
    },
    /// A rainbow spread across each zone's LEDs, rotating.
    Wave {
        #[serde(default = "one")]
        speed: f32,
    },
    /// Colors blended across each zone's LEDs; rotates when speed > 0.
    Gradient {
        colors: Vec<ColorRef>,
        #[serde(default)]
        speed: f32,
    },
    /// Blends from `cold` to `hot` as the source temperature moves from `min` to `max`.
    Temperature {
        source: TempSource,
        cold: ColorRef,
        hot: ColorRef,
        min: f32,
        max: f32,
    },
}

impl Effect {
    pub fn name(&self) -> &'static str {
        match self {
            Effect::Off => "off",
            Effect::Static { .. } => "static",
            Effect::Breathing { .. } => "breathing",
            Effect::Rainbow { .. } => "rainbow",
            Effect::Wave { .. } => "wave",
            Effect::Gradient { .. } => "gradient",
            Effect::Temperature { .. } => "temperature",
        }
    }
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempSource {
    Cpu,
    Gpu,
    Coolant,
    /// The hotter of CPU and GPU.
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FanSpec {
    Fixed { percent: u8 },
    /// Linear interpolation between `(°C, percent)` points; clamped at both ends.
    Curve { source: TempSource, points: Vec<(f32, u8)> },
}

impl FanSpec {
    pub fn evaluate(&self, temp: Option<f32>) -> u8 {
        match self {
            FanSpec::Fixed { percent } => (*percent).min(100),
            FanSpec::Curve { points, .. } => {
                // No reading: fail safe.
                let Some(t) = temp else { return 100 };
                let Some(first) = points.first() else { return 100 };
                if t <= first.0 {
                    return first.1.min(100);
                }
                for w in points.windows(2) {
                    let ((t0, p0), (t1, p1)) = (w[0], w[1]);
                    if t <= t1 {
                        let f = if t1 > t0 { (t - t0) / (t1 - t0) } else { 1.0 };
                        return (p0 as f32 + f * (p1 as f32 - p0 as f32)).round().clamp(0.0, 100.0) as u8;
                    }
                }
                points.last().map_or(100, |p| p.1.min(100))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdSpec {
    pub mode: LcdMode,
    #[serde(default = "hundred")]
    pub brightness: u8,
    /// 0, 90, 180 or 270.
    #[serde(default)]
    pub rotation: u16,
    /// Path to a PNG/JPEG/GIF for `image` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Accent color for gauges.
    #[serde(default = "theme_accent")]
    pub accent: ColorRef,
    /// Layouts shown in turn by `cycle` mode.
    #[serde(default = "default_cycle")]
    pub cycle: Vec<LcdMode>,
    /// Seconds per layout in `cycle` mode.
    #[serde(default = "default_cycle_seconds")]
    pub cycle_seconds: u32,
    /// Size of the plot on the `graph` layout, [`GRAPH_SCALE_MIN`]..=[`GRAPH_SCALE_MAX`].
    #[serde(default = "one")]
    pub graph_scale: f32,
}

pub const GRAPH_SCALE_MIN: f32 = 0.6;
pub const GRAPH_SCALE_MAX: f32 = 1.5;

impl Default for LcdSpec {
    fn default() -> Self {
        Self {
            mode: LcdMode::Coolant,
            brightness: 100,
            rotation: 0,
            image: None,
            accent: theme_accent(),
            cycle: default_cycle(),
            cycle_seconds: default_cycle_seconds(),
            graph_scale: 1.0,
        }
    }
}

fn default_cycle() -> Vec<LcdMode> {
    vec![LcdMode::Coolant, LcdMode::Rings, LcdMode::Load, LcdMode::Cooling, LcdMode::Graph]
}

fn default_cycle_seconds() -> u32 {
    10
}

fn hundred() -> u8 {
    100
}

fn theme_accent() -> ColorRef {
    "theme:accent".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LcdMode {
    /// Large coolant temperature with an arc gauge.
    Coolant,
    /// CPU, GPU and coolant temperatures.
    System,
    /// Concentric CPU (outer) and GPU (inner) temperature rings around the coolant reading.
    Rings,
    /// CPU and GPU utilization.
    Load,
    /// Pump speed with a duty-cycle ring, plus fan summary.
    Cooling,
    /// CPU and GPU temperature history.
    Graph,
    Clock,
    /// Rotates through `cycle` every `cycle_seconds`.
    Cycle,
    Image,
    /// Hand the panel back to its firmware screen.
    Firmware,
    Off,
}

impl LcdMode {
    /// Rendered layouts that can take part in `cycle`.
    pub fn cyclable(self) -> bool {
        !matches!(self, LcdMode::Cycle | LcdMode::Image | LcdMode::Firmware | LcdMode::Off)
    }

    pub const ALL: [LcdMode; 11] = [
        LcdMode::Coolant,
        LcdMode::System,
        LcdMode::Rings,
        LcdMode::Load,
        LcdMode::Cooling,
        LcdMode::Graph,
        LcdMode::Clock,
        LcdMode::Cycle,
        LcdMode::Image,
        LcdMode::Firmware,
        LcdMode::Off,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_interpolates_and_clamps() {
        let c = FanSpec::Curve {
            source: TempSource::Coolant,
            points: vec![(30.0, 40), (40.0, 80)],
        };
        assert_eq!(c.evaluate(Some(20.0)), 40);
        assert_eq!(c.evaluate(Some(35.0)), 60);
        assert_eq!(c.evaluate(Some(50.0)), 80);
        assert_eq!(c.evaluate(None), 100);
    }

    #[test]
    fn default_config_roundtrips_through_json() {
        let c = Config::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.profiles["default"].fans, c.profiles["default"].fans);
    }
}
