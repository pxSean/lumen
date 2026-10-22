//! AIO LCD worker: renders 480×480 frames (gauges, clock, images/GIFs) and streams them.

use std::f32::consts::PI;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, RgbImage};
use lumen_hid::Rgb;
use lumen_hid::icue_lcd::{HEIGHT, Lcd, Rotation, WIDTH};
use lumen_ipc::{LcdMode, LcdSpec, LcdStatus};

use crate::effects::lerp;
use crate::state::Shared;
use crate::theme::Theme;

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const GAUGE_MIN: f32 = 20.0;
const GAUGE_MAX: f32 = 60.0;

pub fn run(shared: Arc<Shared>) {
    let fonts = Fonts::load();
    while shared.running() {
        match session(&shared, &fonts) {
            Ok(()) => break,
            Err(e) => {
                log::error!("LCD: {e:#}");
                shared.runtime().lcd = Some(LcdStatus {
                    connected: false,
                    serial: String::new(),
                    error: Some(format!("{e:#}")),
                });
                let end = Instant::now() + Duration::from_secs(3);
                while shared.running() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

/// Returns the panel to its firmware screen however the session ends.
struct Panel(Lcd);

impl Drop for Panel {
    fn drop(&mut self) {
        if let Err(e) = self.0.release() {
            log::warn!("LCD release failed: {e}");
        }
    }
}

fn session(shared: &Shared, fonts: &Fonts) -> anyhow::Result<()> {
    let panel = Panel(Lcd::open()?);
    let lcd = &panel.0;
    log::info!("AIO LCD {} connected", lcd.serial());
    shared.runtime().lcd = Some(LcdStatus {
        connected: true,
        serial: lcd.serial().to_string(),
        error: None,
    });

    let mut generation = 0;
    let mut spec = LcdSpec::default();
    let mut frames: Vec<(Vec<u8>, Duration)> = Vec::new();
    let mut frame_idx = 0;
    let mut released = false;

    while shared.running() {
        if shared.generation() != generation {
            generation = shared.generation();
            spec = shared.active_profile().lcd;
            frames.clear();
            frame_idx = 0;
            if spec.mode == LcdMode::Firmware {
                lcd.release()?;
                released = true;
            } else {
                released = false;
                let brightness = if spec.mode == LcdMode::Off { 0 } else { spec.brightness };
                lcd.set_brightness(brightness)?;
                lcd.set_rotation(match spec.rotation {
                    90 => Rotation::R90,
                    180 => Rotation::R180,
                    270 => Rotation::R270,
                    _ => Rotation::R0,
                })?;
            }
            if spec.mode == LcdMode::Image {
                frames = match spec.image.as_deref().map(load_image) {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => {
                        log::error!("LCD image: {e:#}");
                        Vec::new()
                    }
                    None => Vec::new(),
                };
            }
        }

        let wait = match spec.mode {
            LcdMode::Firmware => {
                debug_assert!(released);
                Duration::from_millis(500)
            }
            LcdMode::Image if !frames.is_empty() => {
                let (jpeg, delay) = &frames[frame_idx % frames.len()];
                lcd.send_jpeg(jpeg)?;
                frame_idx = frame_idx.wrapping_add(1);
                // Single images still get re-sent periodically.
                if frames.len() == 1 { STATS_INTERVAL } else { *delay }
            }
            mode => {
                let img = {
                    let theme = shared.theme.read().unwrap();
                    let rt = shared.runtime();
                    let pump_rpm = rt
                        .devices
                        .iter()
                        .flat_map(|d| &d.zones)
                        .find(|z| z.kind == "aio" || z.kind == "pump")
                        .and_then(|z| z.rpm);
                    let data = ScreenData { sensors: rt.sensors, pump_rpm };
                    drop(rt);
                    render(mode, &spec, &theme, fonts, &data)
                };
                lcd.send_jpeg(&encode(&img)?)?;
                STATS_INTERVAL
            }
        };

        // Sleep in small steps so config changes apply promptly.
        let end = Instant::now() + wait;
        while shared.running() && shared.generation() == generation && Instant::now() < end {
            std::thread::sleep(wait.min(Duration::from_millis(50)));
        }
    }
    Ok(())
}

fn encode(img: &RgbImage) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(48 * 1024);
    JpegEncoder::new_with_quality(&mut out, 90).encode_image(img)?;
    Ok(out)
}

fn load_image(path: &str) -> anyhow::Result<Vec<(Vec<u8>, Duration)>> {
    let fit = |img: DynamicImage| img.resize_to_fill(WIDTH, HEIGHT, FilterType::Lanczos3).to_rgb8();
    if path.to_ascii_lowercase().ends_with(".gif") {
        let file = std::io::BufReader::new(std::fs::File::open(path)?);
        let frames = GifDecoder::new(file)?.into_frames().collect_frames()?;
        frames
            .into_iter()
            .map(|f| {
                let (num, den) = f.delay().numer_denom_ms();
                let delay = Duration::from_millis((num / den.max(1)).max(20) as u64);
                Ok((encode(&fit(DynamicImage::ImageRgba8(f.into_buffer())))?, delay))
            })
            .collect()
    } else {
        Ok(vec![(encode(&fit(image::open(path)?))?, STATS_INTERVAL)])
    }
}

struct ScreenData {
    sensors: lumen_ipc::Sensors,
    pump_rpm: Option<u16>,
}

pub struct Fonts {
    regular: Option<FontVec>,
    bold: Option<FontVec>,
}

impl Fonts {
    fn load() -> Self {
        let find = |pattern: &str| -> Option<FontVec> {
            let out = Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
            let path = String::from_utf8(out.stdout).ok()?;
            let font = FontVec::try_from_vec(std::fs::read(&path).ok()?).ok()?;
            log::info!("LCD font '{pattern}': {path}");
            Some(font)
        };
        let regular = find("monospace");
        let bold = find("monospace:bold").or_else(|| find("monospace"));
        if regular.is_none() {
            log::warn!("no font found via fc-match; LCD text will be blank");
        }
        Self { regular, bold }
    }
}

fn rgb(c: Rgb) -> image::Rgb<u8> {
    image::Rgb([c.0, c.1, c.2])
}

fn render(mode: LcdMode, spec: &LcdSpec, theme: &Theme, fonts: &Fonts, d: &ScreenData) -> RgbImage {
    let bg = theme.resolve("theme:background");
    let fg = theme.resolve("theme:foreground");
    let muted = theme.colors.get("dark_foreground").copied().unwrap_or(fg.scale(0.5));
    let accent = theme.resolve(&spec.accent);
    let mut img = RgbImage::from_pixel(WIDTH, HEIGHT, rgb(if theme.colors.is_empty() { Rgb::BLACK } else { bg }));
    let cx = WIDTH as f32 / 2.0;
    let fmt = |t: Option<f32>| t.map_or("--".to_string(), |v| format!("{v:.0}°"));

    match mode {
        LcdMode::Coolant => {
            let t = d.sensors.coolant;
            let frac = t.map_or(0.0, |v| (v - GAUGE_MIN) / (GAUGE_MAX - GAUGE_MIN));
            let hot = theme.resolve("theme:red");
            gauge(&mut img, 205.0, 22.0, frac, accent, lerp(accent, hot, (frac - 0.5) * 2.0), muted);
            text(&mut img, fonts.regular.as_ref(), "LIQUID", 34.0, cx, 165.0, muted);
            let big = t.map_or("--".to_string(), |v| format!("{v:.1}°"));
            text(&mut img, fonts.bold.as_ref(), &big, 120.0, cx, 285.0, fg);
            if let Some(rpm) = d.pump_rpm {
                text(&mut img, fonts.regular.as_ref(), &format!("{rpm} RPM"), 32.0, cx, 345.0, muted);
            }
        }
        LcdMode::System => {
            let rows = [
                ("CPU", d.sensors.cpu, 100.0),
                ("GPU", d.sensors.gpu, 100.0),
                ("LIQ", d.sensors.coolant, GAUGE_MAX),
            ];
            for (i, (label, value, max)) in rows.iter().enumerate() {
                let y = 160.0 + i as f32 * 95.0;
                text_left(&mut img, fonts.regular.as_ref(), label, 40.0, 95.0, y, muted);
                text_right(&mut img, fonts.bold.as_ref(), &fmt(*value), 64.0, 385.0, y, fg);
                let frac = value.map_or(0.0, |v| v / max);
                bar(&mut img, 95.0, y + 18.0, 290.0, 8.0, frac, accent, muted.scale(0.5));
            }
        }
        LcdMode::Clock => {
            let now = chrono::Local::now();
            gauge(&mut img, 205.0, 10.0, (now.timestamp() % 60) as f32 / 60.0, accent, accent, muted.scale(0.4));
            text(&mut img, fonts.bold.as_ref(), &now.format("%H:%M").to_string(), 130.0, cx, 280.0, fg);
            text(&mut img, fonts.regular.as_ref(), &now.format("%a %d %b").to_string(), 36.0, cx, 340.0, muted);
            text(&mut img, fonts.regular.as_ref(), &fmt(d.sensors.coolant), 36.0, cx, 160.0, accent);
        }
        LcdMode::Image => {
            text(&mut img, fonts.regular.as_ref(), "no image set", 32.0, cx, 250.0, muted);
        }
        LcdMode::Off => img = RgbImage::new(WIDTH, HEIGHT),
        LcdMode::Firmware => {}
    }
    img
}

fn blend(img: &mut RgbImage, x: i32, y: i32, c: Rgb, a: f32) {
    if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 || a <= 0.0 {
        return;
    }
    let p = img.get_pixel_mut(x as u32, y as u32);
    let mixed = lerp(Rgb(p[0], p[1], p[2]), c, a);
    *p = rgb(mixed);
}

/// 270° arc gauge opening at the bottom; `frac` of it drawn with a from→to color sweep.
#[allow(clippy::too_many_arguments)]
fn gauge(img: &mut RgbImage, radius: f32, thickness: f32, frac: f32, from: Rgb, to: Rgb, track: Rgb) {
    let frac = frac.clamp(0.0, 1.0);
    let (cx, cy) = (WIDTH as f32 / 2.0, HEIGHT as f32 / 2.0);
    let start = 0.75 * PI; // bottom-left, sweeping clockwise
    let sweep = 1.5 * PI;
    let (inner, outer) = (radius - thickness / 2.0, radius + thickness / 2.0);
    for y in 0..HEIGHT as i32 {
        for x in 0..WIDTH as i32 {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let r = (dx * dx + dy * dy).sqrt();
            let cov = (r - inner + 0.5).clamp(0.0, 1.0) * (outer - r + 0.5).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let pos = (dy.atan2(dx) - start).rem_euclid(2.0 * PI) / sweep;
            if pos > 1.0 {
                continue;
            }
            let color = if pos <= frac { lerp(from, to, pos) } else { track };
            blend(img, x, y, color, cov);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn bar(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, frac: f32, fill: Rgb, track: Rgb) {
    let filled = x + w * frac.clamp(0.0, 1.0);
    for py in y as i32..(y + h) as i32 {
        for px in x as i32..(x + w) as i32 {
            blend(img, px, py, if (px as f32) < filled { fill } else { track }, 1.0);
        }
    }
}

fn text_width(font: &FontVec, s: &str, px: f32) -> f32 {
    let f = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for ch in s.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            w += f.kern(p, id);
        }
        w += f.h_advance(id);
        prev = Some(id);
    }
    w
}

fn text_at(img: &mut RgbImage, font: &FontVec, s: &str, px: f32, x0: f32, baseline: f32, color: Rgb) {
    let scale = PxScale::from(px);
    let f = font.as_scaled(scale);
    let mut x = x0;
    let mut prev = None;
    for ch in s.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            x += f.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(x, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let b = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                blend(img, b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32, color, cov);
            });
        }
        x += f.h_advance(id);
        prev = Some(id);
    }
}

fn text(img: &mut RgbImage, font: Option<&FontVec>, s: &str, px: f32, cx: f32, baseline: f32, color: Rgb) {
    if let Some(font) = font {
        let w = text_width(font, s, px);
        text_at(img, font, s, px, cx - w / 2.0, baseline, color);
    }
}

fn text_left(img: &mut RgbImage, font: Option<&FontVec>, s: &str, px: f32, x: f32, baseline: f32, color: Rgb) {
    if let Some(font) = font {
        text_at(img, font, s, px, x, baseline, color);
    }
}

fn text_right(img: &mut RgbImage, font: Option<&FontVec>, s: &str, px: f32, right: f32, baseline: f32, color: Rgb) {
    if let Some(font) = font {
        let w = text_width(font, s, px);
        text_at(img, font, s, px, right - w, baseline, color);
    }
}
