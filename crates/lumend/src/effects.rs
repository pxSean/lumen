//! Lighting effects: pure functions from (effect, time, LED count) to colors.

use std::f32::consts::TAU;

use lumen_hid::Rgb;
use lumen_ipc::{Effect, Sensors, TempSource};

use crate::theme::Theme;

pub struct Ctx<'a> {
    /// Seconds since the daemon started.
    pub t: f32,
    pub theme: &'a Theme,
    pub sensors: Sensors,
}

pub fn temp_for(source: TempSource, s: &Sensors) -> Option<f32> {
    match source {
        TempSource::Cpu => s.cpu,
        TempSource::Gpu => s.gpu,
        TempSource::Coolant => s.coolant,
        TempSource::Max => match (s.cpu, s.gpu) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
    }
}

pub fn render(effect: &Effect, n: usize, ctx: &Ctx) -> Vec<Rgb> {
    let t = ctx.t;
    match effect {
        Effect::Off => vec![Rgb::BLACK; n],
        Effect::Static { color } => vec![ctx.theme.resolve(color); n],
        Effect::Breathing { color, speed } => {
            let level = 0.5 - 0.5 * (TAU * t * speed / 4.0).cos();
            vec![ctx.theme.resolve(color).scale(level); n]
        }
        Effect::Rainbow { speed } => vec![hsv(t * speed * 36.0, 1.0, 1.0); n],
        Effect::Wave { speed } => (0..n)
            .map(|i| hsv(i as f32 / n.max(1) as f32 * 360.0 + t * speed * 90.0, 1.0, 1.0))
            .collect(),
        Effect::Gradient { colors, speed } => {
            let stops: Vec<Rgb> = colors.iter().map(|c| ctx.theme.resolve(c)).collect();
            if stops.is_empty() {
                return vec![Rgb::BLACK; n];
            }
            (0..n)
                .map(|i| {
                    // Cyclic so a rotating gradient wraps seamlessly.
                    let pos = (i as f32 / n.max(1) as f32 + t * speed * 0.25).rem_euclid(1.0);
                    let x = pos * stops.len() as f32;
                    let a = x.floor() as usize % stops.len();
                    lerp(stops[a], stops[(a + 1) % stops.len()], x.fract())
                })
                .collect()
        }
        Effect::Temperature { source, cold, hot, min, max } => {
            let f = match temp_for(*source, &ctx.sensors) {
                Some(v) if max > min => (v - min) / (max - min),
                // Unknown temperature reads as hot so it's noticed.
                _ => 1.0,
            };
            vec![lerp(ctx.theme.resolve(cold), ctx.theme.resolve(hot), f); n]
        }
    }
}

pub fn lerp(a: Rgb, b: Rgb, f: f32) -> Rgb {
    let f = f.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    Rgb(mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

pub fn hsv(h: f32, s: f32, v: f32) -> Rgb {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to = |f: f32| ((f + m) * 255.0).round() as u8;
    Rgb(to(r), to(g), to(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), Rgb(255, 0, 0));
        assert_eq!(hsv(120.0, 1.0, 1.0), Rgb(0, 255, 0));
        assert_eq!(hsv(240.0, 1.0, 1.0), Rgb(0, 0, 255));
    }

    #[test]
    fn temperature_blends() {
        let theme = Theme::default();
        let ctx = Ctx {
            t: 0.0,
            theme: &theme,
            sensors: Sensors { coolant: Some(40.0), ..Default::default() },
        };
        let e = Effect::Temperature {
            source: TempSource::Coolant,
            cold: "#0000ff".into(),
            hot: "#ff0000".into(),
            min: 30.0,
            max: 50.0,
        };
        assert_eq!(render(&e, 1, &ctx), vec![Rgb(128, 0, 128)]);
    }
}
