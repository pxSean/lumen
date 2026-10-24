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
    /// What the panel shows: a screen name, or one of [`SHOW_CYCLE`], [`SHOW_IMAGE`],
    /// [`SHOW_FIRMWARE`], [`SHOW_OFF`]. (`mode` is the pre-screens name of this field.)
    #[serde(alias = "mode")]
    pub show: String,
    #[serde(default = "hundred")]
    pub brightness: u8,
    /// 0, 90, 180 or 270.
    #[serde(default)]
    pub rotation: u16,
    /// Path to a PNG/JPEG/GIF for `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Accent color for gauges and the first graph series.
    #[serde(default = "theme_accent")]
    pub accent: ColorRef,
    /// Configured screens, each a style plus its options.
    #[serde(default = "default_screens")]
    pub screens: Vec<Screen>,
    /// Screen names shown in turn by `cycle`.
    #[serde(default = "default_cycle")]
    pub cycle: Vec<String>,
    /// Seconds per screen in `cycle`.
    #[serde(default = "default_cycle_seconds")]
    pub cycle_seconds: u32,
    /// Image or GIF drawn behind every screen: a path, or [`WALLPAPER`] for the
    /// current Omarchy wallpaper. `None` uses the theme background color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// How far the background is blended toward the theme background color, 0-100,
    /// so readings stay legible.
    #[serde(default = "default_background_dim")]
    pub background_dim: u8,
    /// Pre-screens graph size; folded into graph screens by [`LcdSpec::normalize`].
    #[serde(default, skip_serializing)]
    pub graph_scale: Option<f32>,
}

pub const SHOW_CYCLE: &str = "cycle";
pub const SHOW_IMAGE: &str = "image";
pub const SHOW_FIRMWARE: &str = "firmware";
pub const SHOW_OFF: &str = "off";
/// `show` values that aren't screens; screen names may not use them.
pub const SHOW_SPECIAL: [&str; 4] = [SHOW_CYCLE, SHOW_IMAGE, SHOW_FIRMWARE, SHOW_OFF];

/// `background` value meaning the current Omarchy wallpaper.
pub const WALLPAPER: &str = "wallpaper";

pub const GRAPH_SCALE_MIN: f32 = 0.6;
pub const GRAPH_SCALE_MAX: f32 = 1.5;
/// Graph and bars screens show 1..=MAX_SERIES metrics.
pub const MAX_SERIES: usize = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub name: String,
    #[serde(flatten)]
    pub style: Style,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case")]
pub enum Style {
    /// Large coolant temperature with an arc gauge. Not configurable.
    Coolant,
    /// Pump speed in a duty ring, plus fan summary. Not configurable.
    Cooling,
    Clock,
    /// Two concentric gauges with both values in the middle and an optional footer reading.
    Rings {
        outer: String,
        inner: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        footer: Option<String>,
    },
    /// One row per metric: label, value and a bar.
    Bars { metrics: Vec<String> },
    /// History of each metric over the last 3 minutes.
    Graph {
        metrics: Vec<String>,
        /// Plot size, [`GRAPH_SCALE_MIN`]..=[`GRAPH_SCALE_MAX`].
        #[serde(default = "one")]
        scale: f32,
    },
}

impl Style {
    pub const NAMES: [&'static str; 6] = ["coolant", "cooling", "clock", "rings", "bars", "graph"];

    pub fn name(&self) -> &'static str {
        match self {
            Style::Coolant => "coolant",
            Style::Cooling => "cooling",
            Style::Clock => "clock",
            Style::Rings { .. } => "rings",
            Style::Bars { .. } => "bars",
            Style::Graph { .. } => "graph",
        }
    }

    /// A style with sensible options, carrying metrics over from `from` where they fit.
    pub fn with_name(name: &str, from: &Style) -> Option<Style> {
        let picked = from.metrics();
        let pick = |n: usize, fallback: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = picked.iter().take(n).cloned().collect();
            for f in fallback {
                if v.len() >= n.min(fallback.len()) {
                    break;
                }
                if !v.iter().any(|m| m == f) {
                    v.push(f.to_string());
                }
            }
            v
        };
        Some(match name {
            "coolant" => Style::Coolant,
            "cooling" => Style::Cooling,
            "clock" => Style::Clock,
            "rings" => {
                let v = pick(2, &["cpu.temp", "gpu.temp"]);
                Style::Rings { outer: v[0].clone(), inner: v[1].clone(), footer: Some("coolant.temp".into()) }
            }
            "bars" => Style::Bars { metrics: pick(MAX_SERIES, &["cpu.temp", "gpu.temp", "coolant.temp"]) },
            "graph" => Style::Graph { metrics: pick(MAX_SERIES, &["cpu.temp", "gpu.temp"]), scale: 1.0 },
            _ => return None,
        })
    }

    /// Metrics the style displays (for carrying over between styles).
    pub fn metrics(&self) -> Vec<String> {
        match self {
            Style::Rings { outer, inner, .. } => vec![outer.clone(), inner.clone()],
            Style::Bars { metrics } | Style::Graph { metrics, .. } => metrics.clone(),
            _ => Vec::new(),
        }
    }
}

fn screen(name: &str, style: Style) -> Screen {
    Screen { name: name.into(), style }
}

/// Default screens reuse the pre-screens mode names so older configs keep working.
fn default_screens() -> Vec<Screen> {
    let v = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        screen("coolant", Style::Coolant),
        screen("system", Style::Bars { metrics: v(&["cpu.temp", "gpu.temp", "coolant.temp"]) }),
        screen(
            "rings",
            Style::Rings { outer: "cpu.temp".into(), inner: "gpu.temp".into(), footer: Some("coolant.temp".into()) },
        ),
        screen("load", Style::Bars { metrics: v(&["cpu.load", "gpu.load", "mem.used"]) }),
        screen("cooling", Style::Cooling),
        screen("graph", Style::Graph { metrics: v(&["cpu.temp", "gpu.temp"]), scale: 1.0 }),
        screen("clock", Style::Clock),
    ]
}

impl LcdSpec {
    pub fn screen(&self, name: &str) -> Option<&Screen> {
        self.screens.iter().find(|s| s.name == name)
    }

    /// Folds legacy fields into screens and repairs dangling references.
    pub fn normalize(&mut self) {
        if let Some(scale) = self.graph_scale.take() {
            for s in &mut self.screens {
                if let Style::Graph { scale: sc, .. } = &mut s.style {
                    *sc = scale;
                }
            }
        }
        if self.screens.is_empty() {
            self.screens = default_screens();
        }
        let names: Vec<String> = self.screens.iter().map(|s| s.name.clone()).collect();
        self.cycle.retain(|c| names.contains(c));
        if !SHOW_SPECIAL.contains(&self.show.as_str()) && !names.contains(&self.show) {
            self.show = names[0].clone();
        }
    }

    /// Checks names, references and metric keys; returns the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.screens {
            if s.name.trim().is_empty() || s.name.contains(char::is_whitespace) {
                return Err(format!("screen name '{}' must be a single word", s.name));
            }
            if SHOW_SPECIAL.contains(&s.name.as_str()) {
                return Err(format!("'{}' is reserved and can't be a screen name", s.name));
            }
            if !seen.insert(s.name.as_str()) {
                return Err(format!("two screens are named '{}'", s.name));
            }
            let check = |k: &String| match crate::metric(k) {
                Some(_) => Ok(()),
                None => Err(format!("screen '{}': unknown metric '{k}' (see `lumenctl metrics`)", s.name)),
            };
            match &s.style {
                Style::Rings { outer, inner, footer } => {
                    check(outer)?;
                    check(inner)?;
                    footer.iter().try_for_each(check)?;
                }
                Style::Bars { metrics } | Style::Graph { metrics, .. } => {
                    if metrics.is_empty() || metrics.len() > MAX_SERIES {
                        return Err(format!("screen '{}' needs 1 to {MAX_SERIES} metrics", s.name));
                    }
                    metrics.iter().try_for_each(check)?;
                }
                _ => {}
            }
        }
        if !SHOW_SPECIAL.contains(&self.show.as_str()) && self.screen(&self.show).is_none() {
            return Err(format!("no screen named '{}'", self.show));
        }
        if let Some(bad) = self.cycle.iter().find(|c| self.screen(c).is_none()) {
            return Err(format!("cycle refers to unknown screen '{bad}'"));
        }
        Ok(())
    }
}

fn default_background_dim() -> u8 {
    55
}

impl Default for LcdSpec {
    fn default() -> Self {
        Self {
            show: "coolant".into(),
            brightness: 100,
            rotation: 0,
            image: None,
            accent: theme_accent(),
            screens: default_screens(),
            cycle: default_cycle(),
            cycle_seconds: default_cycle_seconds(),
            background: None,
            background_dim: default_background_dim(),
            graph_scale: None,
        }
    }
}

fn default_cycle() -> Vec<String> {
    ["coolant", "rings", "load", "cooling", "graph"].map(String::from).to_vec()
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
    fn legacy_lcd_config_migrates_to_screens() {
        let old = r#"
            [lcd]
            mode = "graph"
            cycle = ["coolant", "rings", "load"]
            graph_scale = 1.3
        "#;
        #[derive(Deserialize)]
        struct Wrap {
            lcd: LcdSpec,
        }
        let mut lcd = toml::from_str::<Wrap>(old).unwrap().lcd;
        lcd.normalize();
        assert_eq!(lcd.show, "graph");
        assert_eq!(lcd.cycle, ["coolant", "rings", "load"]);
        assert!(matches!(lcd.screen("graph").unwrap().style, Style::Graph { scale, .. } if scale == 1.3));
        assert!(lcd.validate().is_ok());
    }

    #[test]
    fn screens_roundtrip_through_toml() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            lcd: LcdSpec,
        }
        let text = toml::to_string(&Wrap { lcd: LcdSpec::default() }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.lcd, LcdSpec::default());
    }

    #[test]
    fn validation_catches_bad_metrics_and_names() {
        let mut lcd = LcdSpec::default();
        lcd.screens.push(Screen { name: "cycle".into(), style: Style::Clock });
        assert!(lcd.validate().is_err());
        let mut lcd = LcdSpec::default();
        lcd.screens.push(Screen { name: "x".into(), style: Style::Graph { metrics: vec!["nope".into()], scale: 1.0 } });
        assert!(lcd.validate().unwrap_err().contains("unknown metric"));
    }

    #[test]
    fn default_config_roundtrips_through_json() {
        let c = Config::default();
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.profiles["default"].fans, c.profiles["default"].fans);
    }
}
