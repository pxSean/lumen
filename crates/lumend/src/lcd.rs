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
use lumen_ipc::{LcdMode, LcdSpec, LcdStatus, Sensors, ZoneStatus};

use crate::effects::lerp;
use crate::state::{HISTORY_LEN, Runtime, Shared};
use crate::theme::Theme;

const STATS_INTERVAL: Duration = Duration::from_secs(1);
const GAUGE_MIN: f32 = 20.0;
const GAUGE_MAX: f32 = 60.0;
const RING_MIN: f32 = 30.0;
const RING_MAX: f32 = 95.0;

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
                let mode = if mode == LcdMode::Cycle { cycle_mode(&spec, shared.started.elapsed()) } else { mode };
                let img = {
                    let theme = shared.theme.read().unwrap();
                    let data = ScreenData::collect(&shared.runtime());
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

/// Renders every layout with sample data to `<dir>/<mode>.png` (for development).
pub fn preview(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let fonts = Fonts::load();
    let theme = Theme::load();
    let spec = LcdSpec::default();
    let sample = |i: usize| Sensors {
        cpu: Some(48.0 + 15.0 * (i as f32 / 9.0).sin()),
        gpu: Some(41.0 + 6.0 * (i as f32 / 23.0).sin()),
        coolant: Some(33.0 + i as f32 * 0.01),
        cpu_load: Some(23.0),
        gpu_load: Some(61.0),
    };
    let data = ScreenData {
        sensors: sample(HISTORY_LEN - 1),
        pump_rpm: Some(2302),
        pump_duty: Some(65),
        fan_rpm: Some(735),
        fan_duty: Some(36),
        fan_count: 6,
        history: (0..HISTORY_LEN).map(sample).collect(),
    };
    for mode in LcdMode::ALL.into_iter().filter(|m| m.cyclable()) {
        let name = serde_json::to_string(&mode)?.trim_matches('"').to_string();
        render(mode, &spec, &theme, &fonts, &data).save(dir.join(format!("{name}.png")))?;
    }
    for scale in [lumen_ipc::GRAPH_SCALE_MIN, 1.25, lumen_ipc::GRAPH_SCALE_MAX] {
        let spec = LcdSpec { graph_scale: scale, ..spec.clone() };
        render(LcdMode::Graph, &spec, &theme, &fonts, &data).save(dir.join(format!("graph-{scale}.png")))?;
    }
    Ok(())
}

/// The layout `cycle` mode shows right now.
fn cycle_mode(spec: &LcdSpec, elapsed: Duration) -> LcdMode {
    let list: Vec<LcdMode> = spec.cycle.iter().copied().filter(|m| m.cyclable()).collect();
    if list.is_empty() {
        return LcdMode::Coolant;
    }
    let slot = elapsed.as_secs() / spec.cycle_seconds.max(1) as u64;
    list[slot as usize % list.len()]
}

struct ScreenData {
    sensors: Sensors,
    pump_rpm: Option<u16>,
    pump_duty: Option<u8>,
    fan_rpm: Option<u16>,
    fan_duty: Option<u8>,
    fan_count: usize,
    history: Vec<Sensors>,
}

impl ScreenData {
    fn collect(rt: &Runtime) -> Self {
        let zones: Vec<&ZoneStatus> = rt.devices.iter().flat_map(|d| &d.zones).collect();
        let pump = zones.iter().find(|z| z.kind == "aio" || z.kind == "pump");
        let fans: Vec<&&ZoneStatus> = zones.iter().filter(|z| z.kind == "fan" && z.rpm.is_some()).collect();
        let avg = |v: Vec<u32>| (!v.is_empty()).then(|| v.iter().sum::<u32>() / v.len() as u32);
        Self {
            sensors: rt.sensors,
            pump_rpm: pump.and_then(|z| z.rpm),
            pump_duty: pump.and_then(|z| z.duty),
            fan_rpm: avg(fans.iter().filter_map(|z| z.rpm).map(u32::from).collect()).map(|v| v as u16),
            fan_duty: avg(fans.iter().filter_map(|z| z.duty).map(u32::from).collect()).map(|v| v as u8),
            fan_count: fans.len(),
            history: rt.history.iter().copied().collect(),
        }
    }
}

pub struct Fonts {
    regular: Option<FontVec>,
    bold: Option<FontVec>,
}

impl Fonts {
    pub fn load() -> Self {
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
        LcdMode::Rings => {
            let second = theme.resolve("theme:magenta");
            let hot = theme.resolve("theme:red");
            let frac = |t: Option<f32>| t.map_or(0.0, |v| (v - RING_MIN) / (RING_MAX - RING_MIN));
            let (fc, fg_) = (frac(d.sensors.cpu), frac(d.sensors.gpu));
            gauge(&mut img, 212.0, 18.0, fc, accent, lerp(accent, hot, (fc - 0.5) * 2.0), muted.scale(0.6));
            gauge(&mut img, 186.0, 18.0, fg_, second, lerp(second, hot, (fg_ - 0.5) * 2.0), muted.scale(0.6));
            text(&mut img, fonts.regular.as_ref(), "CPU", 26.0, cx, 140.0, muted);
            text(&mut img, fonts.bold.as_ref(), &fmt(d.sensors.cpu), 84.0, cx, 220.0, accent);
            text(&mut img, fonts.regular.as_ref(), "GPU", 26.0, cx, 262.0, muted);
            text(&mut img, fonts.bold.as_ref(), &fmt(d.sensors.gpu), 84.0, cx, 342.0, second);
            let liq = d.sensors.coolant.map_or("--".to_string(), |v| format!("LIQ {v:.1}°"));
            text(&mut img, fonts.regular.as_ref(), &liq, 28.0, cx, 400.0, fg);
        }
        LcdMode::Load => {
            let pct = |v: Option<f32>| v.map_or("--".to_string(), |v| format!("{v:.0}%"));
            let rows = [
                ("CPU", d.sensors.cpu_load, d.sensors.cpu, accent),
                ("GPU", d.sensors.gpu_load, d.sensors.gpu, theme.resolve("theme:magenta")),
            ];
            for (i, (label, load, temp, color)) in rows.into_iter().enumerate() {
                let y = 190.0 + i as f32 * 120.0;
                text_left(&mut img, fonts.regular.as_ref(), label, 36.0, 90.0, y, muted);
                text_right(&mut img, fonts.bold.as_ref(), &pct(load), 76.0, 390.0, y, fg);
                bar(&mut img, 90.0, y + 20.0, 300.0, 12.0, load.unwrap_or(0.0) / 100.0, color, muted.scale(0.5));
                text_left(&mut img, fonts.regular.as_ref(), &fmt(temp), 24.0, 90.0, y + 58.0, muted);
            }
            let liq = d.sensors.coolant.map_or("--".to_string(), |v| format!("LIQ {v:.1}°"));
            text(&mut img, fonts.regular.as_ref(), &liq, 28.0, cx, 418.0, accent);
        }
        LcdMode::Cooling => {
            let duty = d.pump_duty.map_or(0.0, |p| p as f32 / 100.0);
            gauge(&mut img, 205.0, 22.0, duty, accent, accent, muted.scale(0.6));
            text(&mut img, fonts.regular.as_ref(), "PUMP", 32.0, cx, 155.0, muted);
            let rpm = d.pump_rpm.map_or("--".to_string(), |r| r.to_string());
            text(&mut img, fonts.bold.as_ref(), &rpm, 110.0, cx, 265.0, fg);
            let duty_txt = d.pump_duty.map_or("RPM".to_string(), |p| format!("RPM · {p}%"));
            text(&mut img, fonts.regular.as_ref(), &duty_txt, 28.0, cx, 305.0, muted);
            if let Some(rpm) = d.fan_rpm {
                let fans = format!("{}× FANS {rpm} · {}%", d.fan_count, d.fan_duty.unwrap_or(0));
                text(&mut img, fonts.regular.as_ref(), &fans, 28.0, cx, 370.0, accent);
            }
        }
        LcdMode::Graph => {
            // Same colors as Rings: CPU in the accent, GPU in magenta.
            let gpu_color = theme.resolve("theme:magenta");
            let g = graph_layout(spec.graph_scale);
            text(&mut img, fonts.regular.as_ref(), "CPU", g.label_px, cx - 85.0, g.label_y, muted);
            text(&mut img, fonts.bold.as_ref(), &fmt(d.sensors.cpu), g.value_px, cx - 85.0, g.value_y, accent);
            text(&mut img, fonts.regular.as_ref(), "GPU", g.label_px, cx + 85.0, g.label_y, muted);
            text(&mut img, fonts.bold.as_ref(), &fmt(d.sensors.gpu), g.value_px, cx + 85.0, g.value_y, gpu_color);
            let (x0, x1, y0, y1) = (g.x0, g.x1, g.y0, g.y1);
            let series = |f: fn(&Sensors) -> Option<f32>| -> Vec<(usize, f32)> {
                d.history.iter().enumerate().filter_map(|(i, s)| f(s).map(|v| (i, v))).collect()
            };
            let cpu = series(|s| s.cpu);
            let gpu = series(|s| s.gpu);
            let values = cpu.iter().chain(&gpu).map(|p| p.1);
            let (lo, hi) = values.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
            let (lo, hi) = if lo > hi { (20.0, 60.0) } else { (lo.floor() - 2.0, hi.ceil() + 2.0) };
            let (lo, hi) = if hi - lo < 10.0 { (lo, lo + 10.0) } else { (lo, hi) };
            let n = d.history.len();
            let to_xy = |&(i, v): &(usize, f32)| {
                // Newest sample at the right edge; the plot fills in from the right over HISTORY_LEN s.
                let age = (n - 1 - i) as f32;
                (x1 - age / (HISTORY_LEN - 1) as f32 * (x1 - x0), y1 - (v - lo) / (hi - lo) * (y1 - y0))
            };
            for gy in [y0, (y0 + y1) / 2.0, y1] {
                bar(&mut img, x0, gy, x1 - x0, 1.0, 0.0, muted, muted.scale(0.4));
            }
            let cpu_pts: Vec<(f32, f32)> = cpu.iter().map(to_xy).collect();
            fill_under(&mut img, &cpu_pts, y1, accent, 0.15);
            polyline(&mut img, &gpu.iter().map(to_xy).collect::<Vec<_>>(), gpu_color, 3.0);
            polyline(&mut img, &cpu_pts, accent, 3.0);
            // Axis labels sit inside the plot so a larger plot doesn't push them off the round panel.
            text_left(&mut img, fonts.regular.as_ref(), &format!("{hi:.0}°"), 20.0, x0 + 4.0, y0 + 20.0, muted);
            text_left(&mut img, fonts.regular.as_ref(), &format!("{lo:.0}°"), 20.0, x0 + 4.0, y1 - 6.0, muted);
            text_right(&mut img, fonts.regular.as_ref(), "3 min", 20.0, x1 - 4.0, y1 - 6.0, muted);
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
        // Resolved by the caller / not rendered.
        LcdMode::Cycle | LcdMode::Firmware => {}
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

/// Where the graph layout's header and plot go for a given `graph_scale`.
struct GraphLayout {
    label_px: f32,
    label_y: f32,
    value_px: f32,
    value_y: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
}

/// Radius the plot's corners must stay within on the round panel.
const SAFE_RADIUS: f32 = 226.0;

fn graph_layout(scale: f32) -> GraphLayout {
    let s = scale.clamp(lumen_ipc::GRAPH_SCALE_MIN, lumen_ipc::GRAPH_SCALE_MAX);
    // The header shrinks as the plot grows.
    let shrink = ((s - 1.0) / (lumen_ipc::GRAPH_SCALE_MAX - 1.0)).clamp(0.0, 1.0);
    let label_px = 26.0 - 6.0 * shrink;
    let value_px = 60.0 - 22.0 * shrink;
    let header = label_px + 8.0 + value_px;
    let gap = 22.0;
    let height = 160.0 * s;
    // Center header + plot vertically.
    let top = 240.0 - (header + gap + height) / 2.0;
    let label_y = top + label_px;
    let value_y = label_y + 8.0 + value_px * 0.95;
    let (y0, y1) = (top + header + gap, top + header + gap + height);
    // Widest box whose corners stay inside the circle at both plot edges.
    let dy = (y0 - 240.0).abs().max((y1 - 240.0).abs());
    let fit = 2.0 * (SAFE_RADIUS * SAFE_RADIUS - dy * dy).max(0.0).sqrt();
    let width = (360.0 * s).min(fit);
    GraphLayout { label_px, label_y, value_px, value_y, x0: 240.0 - width / 2.0, x1: 240.0 + width / 2.0, y0, y1 }
}

/// Anti-aliased line through `pts` with round joins.
fn polyline(img: &mut RgbImage, pts: &[(f32, f32)], color: Rgb, width: f32) {
    // Accumulate max coverage per pixel first so overlapping stamps blend exactly once.
    let mut mask = vec![0f32; (WIDTH * HEIGHT) as usize];
    let r = width / 2.0;
    let mut stamp = |px: f32, py: f32| {
        for y in ((py - r - 1.0) as i32).max(0)..=((py + r + 1.0) as i32).min(HEIGHT as i32 - 1) {
            for x in ((px - r - 1.0) as i32).max(0)..=((px + r + 1.0) as i32).min(WIDTH as i32 - 1) {
                let dist = (x as f32 + 0.5 - px).hypot(y as f32 + 0.5 - py);
                let m = &mut mask[(y as u32 * WIDTH + x as u32) as usize];
                *m = m.max((r - dist + 0.5).clamp(0.0, 1.0));
            }
        }
    };
    if let [only] = pts {
        stamp(only.0, only.1);
    }
    for w in pts.windows(2) {
        let ((ax, ay), (bx, by)) = (w[0], w[1]);
        let steps = ((bx - ax).hypot(by - ay) / 0.75).ceil().max(1.0) as usize;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            stamp(ax + (bx - ax) * t, ay + (by - ay) * t);
        }
    }
    for (i, &cov) in mask.iter().enumerate().filter(|(_, c)| **c > 0.0) {
        blend(img, (i as u32 % WIDTH) as i32, (i as u32 / WIDTH) as i32, color, cov);
    }
}

/// Translucent fill between a polyline and the horizontal line `base`.
fn fill_under(img: &mut RgbImage, pts: &[(f32, f32)], base: f32, color: Rgb, alpha: f32) {
    for w in pts.windows(2) {
        let ((ax, ay), (bx, by)) = (w[0], w[1]);
        for x in ax.round() as i32..bx.round() as i32 {
            let t = if bx > ax { (x as f32 - ax) / (bx - ax) } else { 0.0 };
            let top = ay + (by - ay) * t;
            for y in top.round() as i32..base as i32 {
                blend(img, x, y, color, alpha);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_fits_the_round_panel_at_every_scale() {
        let mut s = lumen_ipc::GRAPH_SCALE_MIN;
        let mut prev_height = 0.0;
        while s <= lumen_ipc::GRAPH_SCALE_MAX + 1e-3 {
            let g = graph_layout(s);
            for (x, y) in [(g.x0, g.y0), (g.x1, g.y0), (g.x0, g.y1), (g.x1, g.y1)] {
                let r = (x - 240.0).hypot(y - 240.0);
                assert!(r <= SAFE_RADIUS + 0.01, "scale {s}: corner ({x}, {y}) at r={r}");
            }
            assert!(g.label_y - g.label_px >= 240.0 - SAFE_RADIUS, "scale {s}: header clipped");
            assert!(g.y1 - g.y0 > prev_height, "scale {s}: plot did not grow");
            prev_height = g.y1 - g.y0;
            s += 0.1;
        }
    }

    #[test]
    fn cycle_skips_unrenderable_and_wraps() {
        let spec = LcdSpec {
            cycle: vec![LcdMode::Rings, LcdMode::Image, LcdMode::Graph],
            cycle_seconds: 10,
            ..LcdSpec::default()
        };
        assert_eq!(cycle_mode(&spec, Duration::from_secs(5)), LcdMode::Rings);
        assert_eq!(cycle_mode(&spec, Duration::from_secs(15)), LcdMode::Graph);
        assert_eq!(cycle_mode(&spec, Duration::from_secs(25)), LcdMode::Rings);
        let empty = LcdSpec { cycle: vec![LcdMode::Off], ..LcdSpec::default() };
        assert_eq!(cycle_mode(&empty, Duration::ZERO), LcdMode::Coolant);
    }
}
