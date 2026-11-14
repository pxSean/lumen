//! Catalog of the metrics lumen-icued collects. Screens refer to metrics by key.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Celsius,
    Percent,
    Rpm,
    Watts,
}

impl Unit {
    /// Compact value for screens and lists: `61°`, `23%`, `2302`, `215W`.
    pub fn format(self, v: f32) -> String {
        match self {
            Unit::Celsius => format!("{v:.0}°"),
            Unit::Percent => format!("{v:.0}%"),
            Unit::Rpm => format!("{v:.0}"),
            Unit::Watts => format!("{v:.0}W"),
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Unit::Celsius => "°C",
            Unit::Percent => "%",
            Unit::Rpm => "rpm",
            Unit::Watts => "W",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MetricInfo {
    pub key: &'static str,
    /// Short label for screens (unit disambiguates CPU temp vs CPU load).
    pub label: &'static str,
    pub description: &'static str,
    pub unit: Unit,
    /// Nominal range for gauges, bars and mixed-unit graphs.
    pub min: f32,
    pub max: f32,
}

const fn m(key: &'static str, label: &'static str, description: &'static str, unit: Unit, min: f32, max: f32) -> MetricInfo {
    MetricInfo { key, label, description, unit, min, max }
}

pub const METRICS: &[MetricInfo] = &[
    m("cpu.temp", "CPU", "CPU temperature", Unit::Celsius, 30.0, 95.0),
    m("cpu.load", "CPU", "CPU utilization", Unit::Percent, 0.0, 100.0),
    m("gpu.temp", "GPU", "GPU temperature (discrete when present)", Unit::Celsius, 30.0, 90.0),
    m("gpu.load", "GPU", "GPU utilization (not on Intel)", Unit::Percent, 0.0, 100.0),
    m("gpu.power", "GPU", "GPU power draw", Unit::Watts, 0.0, 350.0),
    m("gpu.vram", "VRAM", "GPU memory used (NVIDIA, AMD)", Unit::Percent, 0.0, 100.0),
    m("igpu.temp", "iGPU", "AMD integrated GPU temperature, beside another GPU", Unit::Celsius, 30.0, 90.0),
    m("igpu.load", "iGPU", "AMD integrated GPU utilization, beside another GPU", Unit::Percent, 0.0, 100.0),
    m("mem.used", "RAM", "System memory used", Unit::Percent, 0.0, 100.0),
    m("coolant.temp", "LIQ", "AIO coolant temperature", Unit::Celsius, 20.0, 60.0),
    m("pump.rpm", "PUMP", "Pump speed", Unit::Rpm, 0.0, 3000.0),
    m("pump.duty", "PUMP", "Pump duty cycle", Unit::Percent, 0.0, 100.0),
    m("fans.rpm", "FANS", "Average fan speed", Unit::Rpm, 0.0, 2500.0),
    m("fans.duty", "FANS", "Average fan duty cycle", Unit::Percent, 0.0, 100.0),
];

pub fn metric(key: &str) -> Option<&'static MetricInfo> {
    METRICS.iter().find(|m| m.key == key)
}

/// Formats a metric value, or `--` when missing.
pub fn format_metric(key: &str, value: Option<f32>) -> String {
    match (metric(key), value) {
        (Some(info), Some(v)) => info.unit.format(v),
        (None, Some(v)) => format!("{v:.0}"),
        _ => "--".into(),
    }
}
