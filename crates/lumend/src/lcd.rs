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
use std::collections::BTreeMap;

use lumen_ipc::{
    LcdSpec, LcdStatus, MAX_SERIES, SHOW_CYCLE, SHOW_FIRMWARE, SHOW_IMAGE, SHOW_OFF, Screen, Style, Unit, format_metric, metric,
};

use crate::effects::lerp;
use crate::state::{HISTORY_LEN, Runtime, Shared};
use crate::theme::Theme;

const STATS_INTERVAL: Duration = Duration::from_secs(1);

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
    let mut background: Vec<(RgbImage, Duration)> = Vec::new();
    let mut bg_idx = 0;

    while shared.running() {
        if shared.generation() != generation {
            generation = shared.generation();
            spec = shared.active_profile().lcd;
            frames.clear();
            frame_idx = 0;
            let show = spec.show.as_str();
            if show == SHOW_FIRMWARE {
                lcd.release()?;
            } else {
                let brightness = if show == SHOW_OFF { 0 } else { spec.brightness };
                lcd.set_brightness(brightness)?;
                lcd.set_rotation(match spec.rotation {
                    90 => Rotation::R90,
                    180 => Rotation::R180,
                    270 => Rotation::R270,
                    _ => Rotation::R0,
                })?;
            }
            background.clear();
            bg_idx = 0;
            if !matches!(show, SHOW_IMAGE | SHOW_OFF | SHOW_FIRMWARE) {
                let theme = shared.theme.read().unwrap().clone();
                background = load_background(&spec, &theme).unwrap_or_else(|e| {
                    log::error!("LCD background: {e:#}");
                    Vec::new()
                });
            }
            if show == SHOW_IMAGE {
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

        let wait = match spec.show.as_str() {
            SHOW_FIRMWARE => Duration::from_millis(500),
            SHOW_OFF => {
                lcd.send_jpeg(&encode(&RgbImage::new(WIDTH, HEIGHT))?)?;
                STATS_INTERVAL
            }
            SHOW_IMAGE if !frames.is_empty() => {
                let (jpeg, delay) = &frames[frame_idx % frames.len()];
                lcd.send_jpeg(jpeg)?;
                frame_idx = frame_idx.wrapping_add(1);
                // Single images still get re-sent periodically.
                if frames.len() == 1 { STATS_INTERVAL } else { *delay }
            }
            show => {
                let screen = match show {
                    SHOW_CYCLE => cycle_screen(&spec, shared.started.elapsed()),
                    // An image with nothing loaded falls through to a placeholder.
                    SHOW_IMAGE => None,
                    name => spec.screen(name),
                };
                let base = background.get(bg_idx % background.len().max(1));
                let img = {
                    let theme = shared.theme.read().unwrap();
                    let data = ScreenData::collect(&shared.runtime());
                    match screen {
                        Some(screen) => render(screen, &spec, &theme, fonts, &data, base.map(|b| &b.0)),
                        None => placeholder(&theme, fonts, if show == SHOW_IMAGE { "no image set" } else { "no screens" }),
                    }
                };
                lcd.send_jpeg(&encode(&img)?)?;
                match base {
                    Some((_, delay)) if background.len() > 1 => {
                        bg_idx = bg_idx.wrapping_add(1);
                        (*delay).max(MIN_BACKGROUND_FRAME)
                    }
                    _ => STATS_INTERVAL,
                }
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

/// Decodes an image or GIF into 480×480 frames (center-cropped) with per-frame delays.
fn decode_frames(path: &str) -> anyhow::Result<Vec<(RgbImage, Duration)>> {
    let fit = |img: DynamicImage| img.resize_to_fill(WIDTH, HEIGHT, FilterType::Lanczos3).to_rgb8();
    // Resolve symlinks (the Omarchy wallpaper is one, with no extension) and sniff the format.
    let path = std::fs::canonicalize(path)?;
    let reader = image::ImageReader::open(&path)?.with_guessed_format()?;
    if reader.format() == Some(image::ImageFormat::Gif) {
        let file = std::io::BufReader::new(std::fs::File::open(&path)?);
        let frames = GifDecoder::new(file)?.into_frames().collect_frames()?;
        Ok(frames
            .into_iter()
            .map(|f| {
                let (num, den) = f.delay().numer_denom_ms();
                let delay = Duration::from_millis((num / den.max(1)).max(20) as u64);
                (fit(DynamicImage::ImageRgba8(f.into_buffer())), delay)
            })
            .collect())
    } else {
        Ok(vec![(fit(reader.decode()?), STATS_INTERVAL)])
    }
}

/// Image mode: frames are pre-encoded once and replayed.
fn load_image(path: &str) -> anyhow::Result<Vec<(Vec<u8>, Duration)>> {
    decode_frames(path)?.into_iter().map(|(img, d)| Ok((encode(&img)?, d))).collect()
}

/// Background frames are kept decoded (≈690 KB each) so layouts can be drawn on top;
/// longer GIFs are thinned to stay under this many frames.
const MAX_BACKGROUND_FRAMES: usize = 150;
/// Animated backgrounds redraw the layout every frame; cap the rate to bound CPU use.
const MIN_BACKGROUND_FRAME: Duration = Duration::from_millis(50);

fn background_path(spec: &LcdSpec) -> Option<String> {
    let bg = spec.background.as_deref()?;
    if bg == lumen_ipc::WALLPAPER {
        let home = std::env::var("HOME").unwrap_or_default();
        Some(format!("{home}/.local/state/omarchy/current/background"))
    } else {
        Some(bg.to_string())
    }
}

/// Decodes the background and blends it toward the theme background by `background_dim`.
fn load_background(spec: &LcdSpec, theme: &Theme) -> anyhow::Result<Vec<(RgbImage, Duration)>> {
    let Some(path) = background_path(spec) else { return Ok(Vec::new()) };
    let mut frames = decode_frames(&path)?;
    if frames.len() > MAX_BACKGROUND_FRAMES {
        let step = frames.len().div_ceil(MAX_BACKGROUND_FRAMES);
        log::info!("LCD background: thinning {} frames to every {step}th", frames.len());
        frames = frames
            .into_iter()
            .step_by(step)
            .map(|(img, d)| (img, d * step as u32))
            .collect();
    }
    let toward = if theme.colors.is_empty() { Rgb::BLACK } else { theme.resolve("theme:background") };
    let f = spec.background_dim.min(100) as f32 / 100.0;
    for (img, _) in &mut frames {
        for p in img.pixels_mut() {
            *p = rgb(lerp(Rgb(p[0], p[1], p[2]), toward, f));
        }
    }
    Ok(frames)
}

/// Deterministic sensor-like noise in roughly [-1, 1] for preview data: slow drift
/// plus faster wobble plus per-sample jitter, so graphs look like real readings
/// and every preview run renders the same image.
fn preview_noise(seed: u32, t: f32) -> f32 {
    fn hash(seed: u32, k: i64) -> f32 {
        let mut x = (k as u64 as u32) ^ seed.wrapping_mul(0x9E37_79B9);
        x ^= x >> 16;
        x = x.wrapping_mul(0x7FEB_352D);
        x ^= x >> 15;
        x = x.wrapping_mul(0x846C_A68B);
        x ^= x >> 16;
        x as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
    fn value(seed: u32, t: f32) -> f32 {
        let k = t.floor();
        let f = t - k;
        let s = f * f * (3.0 - 2.0 * f);
        let k = k as i64;
        hash(seed, k) * (1.0 - s) + hash(seed, k + 1) * s
    }
    0.6 * value(seed, t / 14.0) + 0.3 * value(seed.wrapping_add(101), t / 4.0) + 0.1 * hash(seed.wrapping_add(202), t as i64)
}

/// Renders every default screen plus a few metric combinations with sample data to
/// `<dir>/<name>.png` (for development).
pub fn preview(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let fonts = Fonts::load();
    let theme = Theme::load();
    let spec = LcdSpec::default();
    let sample = |i: usize| -> BTreeMap<String, f32> {
        let t = i as f32;
        [
            ("cpu.temp", 48.0 + 12.0 * preview_noise(1, t)),
            ("cpu.load", (30.0 + 24.0 * preview_noise(2, t)).clamp(0.0, 100.0)),
            ("gpu.temp", 58.0 + 6.0 * preview_noise(3, t)),
            ("gpu.load", (86.0 + 12.0 * preview_noise(4, t)).clamp(0.0, 100.0)),
            ("gpu.power", 240.0 + 35.0 * preview_noise(5, t)),
            ("gpu.vram", 62.0),
            ("igpu.temp", 38.0 + 1.5 * preview_noise(6, t)),
            ("igpu.load", 2.0),
            ("mem.used", 41.0 + t * 0.02 + 1.5 * preview_noise(7, t)),
            ("coolant.temp", 33.0 + t * 0.01),
            ("pump.rpm", 2302.0),
            ("pump.duty", 65.0),
            ("fans.rpm", 735.0),
            ("fans.duty", 36.0),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    };
    let data = ScreenData {
        metrics: sample(HISTORY_LEN - 1),
        history: (0..HISTORY_LEN).map(sample).collect(),
        fan_count: 6,
    };
    let v = |m: &[&str]| m.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut screens = spec.screens.clone();
    screens.extend([
        Screen { name: "graph-gpus".into(), style: Style::Graph { metrics: v(&["gpu.temp", "igpu.temp", "cpu.temp"]), scale: 1.0, lanes: false } },
        Screen { name: "graph-mixed".into(), style: Style::Graph { metrics: v(&["cpu.load", "gpu.load", "gpu.power", "mem.used"]), scale: 1.2, lanes: false } },
        Screen { name: "graph-lanes".into(), style: Style::Graph { metrics: v(&["cpu.temp", "gpu.temp", "mem.used"]), scale: 1.0, lanes: true } },
        Screen { name: "graph-lanes-3".into(), style: Style::Graph { metrics: v(&["cpu.temp", "gpu.load", "gpu.power", "mem.used"]), scale: 1.2, lanes: true } },
        Screen { name: "bars-4".into(), style: Style::Bars { metrics: v(&["gpu.temp", "gpu.load", "gpu.power", "gpu.vram"]) } },
        Screen { name: "rings-load".into(), style: Style::Rings { outer: "cpu.load".into(), inner: "gpu.load".into(), footer: Some("mem.used".into()) } },
    ]);
    for screen in &screens {
        render(screen, &spec, &theme, &fonts, &data, None).save(dir.join(format!("{}.png", screen.name)))?;
    }
    let wallpaper = LcdSpec { background: Some(lumen_ipc::WALLPAPER.into()), ..spec.clone() };
    match load_background(&wallpaper, &theme) {
        Ok(bg) if !bg.is_empty() => {
            for name in ["coolant", "rings", "graph"] {
                let screen = spec.screen(name).expect("default screen");
                render(screen, &wallpaper, &theme, &fonts, &data, Some(&bg[0].0)).save(dir.join(format!("bg-{name}.png")))?;
            }
        }
        Ok(_) => {}
        Err(e) => log::warn!("preview: no wallpaper background: {e:#}"),
    }
    Ok(())
}

/// The screen `cycle` shows right now.
fn cycle_screen(spec: &LcdSpec, elapsed: Duration) -> Option<&Screen> {
    let list: Vec<&Screen> = spec.cycle.iter().filter_map(|n| spec.screen(n)).collect();
    if list.is_empty() {
        return spec.screens.first();
    }
    let slot = elapsed.as_secs() / spec.cycle_seconds.max(1) as u64;
    Some(list[slot as usize % list.len()])
}

struct ScreenData {
    metrics: BTreeMap<String, f32>,
    history: Vec<BTreeMap<String, f32>>,
    fan_count: usize,
}

impl ScreenData {
    fn collect(rt: &Runtime) -> Self {
        let fan_count = rt
            .devices
            .iter()
            .flat_map(|d| &d.zones)
            .filter(|z| z.kind == "fan" && z.rpm.is_some())
            .count();
        Self { metrics: rt.metrics.clone(), history: rt.history.iter().cloned().collect(), fan_count }
    }

    fn get(&self, key: &str) -> Option<f32> {
        self.metrics.get(key).copied()
    }

    fn fmt(&self, key: &str) -> String {
        format_metric(key, self.get(key))
    }

    /// Position of the value within the metric's nominal range, 0..1.
    fn frac(&self, key: &str) -> f32 {
        match (metric(key), self.get(key)) {
            (Some(i), Some(v)) if i.max > i.min => ((v - i.min) / (i.max - i.min)).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

fn label(key: &str) -> &'static str {
    metric(key).map_or("?", |m| m.label)
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

fn blank(theme: &Theme) -> RgbImage {
    let bg = if theme.colors.is_empty() { Rgb::BLACK } else { theme.resolve("theme:background") };
    RgbImage::from_pixel(WIDTH, HEIGHT, rgb(bg))
}

fn placeholder(theme: &Theme, fonts: &Fonts, msg: &str) -> RgbImage {
    let mut img = blank(theme);
    let muted = theme.colors.get("dark_foreground").copied().unwrap_or(Rgb(128, 128, 128));
    text(&mut img, fonts.regular.as_ref(), msg, 32.0, WIDTH as f32 / 2.0, 250.0, muted);
    img
}

/// Series colors for graphs, bars and rings, in order.
fn series_colors(spec: &LcdSpec, theme: &Theme) -> [Rgb; MAX_SERIES] {
    [
        theme.resolve(&spec.accent),
        theme.resolve("theme:magenta"),
        theme.resolve("theme:yellow"),
        theme.resolve("theme:green"),
    ]
}

/// Draws `screen` over `base` (a pre-dimmed background frame) or the theme background color.
fn render(screen: &Screen, spec: &LcdSpec, theme: &Theme, fonts: &Fonts, d: &ScreenData, base: Option<&RgbImage>) -> RgbImage {
    let fg = theme.resolve("theme:foreground");
    let muted = theme.colors.get("dark_foreground").copied().unwrap_or(fg.scale(0.5));
    let colors = series_colors(spec, theme);
    let accent = colors[0];
    let hot = theme.resolve("theme:red");
    let mut img = base.cloned().unwrap_or_else(|| blank(theme));
    let cx = WIDTH as f32 / 2.0;

    match &screen.style {
        Style::Coolant => {
            let frac = d.frac("coolant.temp");
            gauge(&mut img, 205.0, 22.0, frac, accent, lerp(accent, hot, (frac - 0.5) * 2.0), muted);
            text(&mut img, fonts.regular.as_ref(), "LIQUID", 34.0, cx, 165.0, muted);
            let big = d.get("coolant.temp").map_or("--".to_string(), |v| format!("{v:.1}°"));
            text(&mut img, fonts.bold.as_ref(), &big, 120.0, cx, 285.0, fg);
            if let Some(rpm) = d.get("pump.rpm") {
                text(&mut img, fonts.regular.as_ref(), &format!("{rpm:.0} RPM"), 32.0, cx, 345.0, muted);
            }
        }
        Style::Cooling => {
            gauge(&mut img, 205.0, 22.0, d.frac("pump.duty"), accent, accent, muted.scale(0.6));
            text(&mut img, fonts.regular.as_ref(), "PUMP", 32.0, cx, 155.0, muted);
            text(&mut img, fonts.bold.as_ref(), &d.fmt("pump.rpm"), 110.0, cx, 265.0, fg);
            let duty = d.get("pump.duty").map_or("RPM".to_string(), |p| format!("RPM · {p:.0}%"));
            text(&mut img, fonts.regular.as_ref(), &duty, 28.0, cx, 305.0, muted);
            if let Some(rpm) = d.get("fans.rpm") {
                let fans = format!("{}× FANS {rpm:.0} · {}", d.fan_count, d.fmt("fans.duty"));
                text(&mut img, fonts.regular.as_ref(), &fans, 28.0, cx, 370.0, accent);
            }
        }
        Style::Clock => {
            let now = chrono::Local::now();
            gauge(&mut img, 205.0, 10.0, (now.timestamp() % 60) as f32 / 60.0, accent, accent, muted.scale(0.4));
            text(&mut img, fonts.bold.as_ref(), &now.format("%H:%M").to_string(), 130.0, cx, 280.0, fg);
            text(&mut img, fonts.regular.as_ref(), &now.format("%a %d %b").to_string(), 36.0, cx, 340.0, muted);
            text(&mut img, fonts.regular.as_ref(), &d.fmt("coolant.temp"), 36.0, cx, 160.0, accent);
        }
        Style::Rings { outer, inner, footer } => {
            let (fo, fi) = (d.frac(outer), d.frac(inner));
            let second = colors[1];
            gauge(&mut img, 212.0, 18.0, fo, accent, lerp(accent, hot, (fo - 0.5) * 2.0), muted.scale(0.6));
            gauge(&mut img, 186.0, 18.0, fi, second, lerp(second, hot, (fi - 0.5) * 2.0), muted.scale(0.6));
            text(&mut img, fonts.regular.as_ref(), label(outer), 26.0, cx, 140.0, muted);
            text(&mut img, fonts.bold.as_ref(), &d.fmt(outer), 84.0, cx, 220.0, accent);
            text(&mut img, fonts.regular.as_ref(), label(inner), 26.0, cx, 262.0, muted);
            text(&mut img, fonts.bold.as_ref(), &d.fmt(inner), 84.0, cx, 342.0, second);
            if let Some(f) = footer {
                let line = format!("{} {}", label(f), d.fmt(f));
                text(&mut img, fonts.regular.as_ref(), &line, 28.0, cx, 400.0, fg);
            }
        }
        Style::Bars { metrics } => {
            let n = metrics.len().clamp(1, MAX_SERIES);
            let (row, value_px, label_px) = if n <= 3 { (95.0, 64.0, 40.0) } else { (78.0, 52.0, 34.0) };
            let first = 255.0 - (n as f32 - 1.0) * row / 2.0;
            for (i, key) in metrics.iter().take(n).enumerate() {
                let y = first + i as f32 * row;
                text_left(&mut img, fonts.regular.as_ref(), label(key), label_px, 95.0, y, muted);
                text_right(&mut img, fonts.bold.as_ref(), &d.fmt(key), value_px, 385.0, y, fg);
                bar(&mut img, 95.0, y + 18.0, 290.0, 8.0, d.frac(key), colors[i], muted.scale(0.5));
            }
        }
        Style::Graph { metrics, scale, lanes } => draw_graph(&mut img, metrics, *scale, *lanes, &colors, fonts, d, muted),
    }
    img
}

/// History plot. Series sharing one unit share an auto-fitted axis; mixed units are each
/// scaled to their metric's nominal range and the axis labels are omitted. With `lanes`,
/// each unit instead gets its own horizontal lane with its own auto-fitted, labelled axis.
#[allow(clippy::too_many_arguments)]
fn draw_graph(img: &mut RgbImage, metrics: &[String], scale: f32, lanes: bool, colors: &[Rgb; MAX_SERIES], fonts: &Fonts, d: &ScreenData, muted: Rgb) {
    let metrics = &metrics[..metrics.len().min(MAX_SERIES)];
    let n = metrics.len().max(1);
    let g = graph_layout(scale);
    let cx = WIDTH as f32 / 2.0;

    // Header: one current value per series, in its color.
    let (xs, shrink): (&[f32], f32) = match n {
        1 => (&[0.0], 1.0),
        2 => (&[-85.0, 85.0], 1.0),
        3 => (&[-125.0, 0.0, 125.0], 0.82),
        _ => (&[-153.0, -51.0, 51.0, 153.0], 0.66),
    };
    for (i, key) in metrics.iter().enumerate() {
        let x = cx + xs[i];
        text(img, fonts.regular.as_ref(), label(key), g.label_px * shrink.max(0.8), x, g.label_y, muted);
        text(img, fonts.bold.as_ref(), &d.fmt(key), g.value_px * shrink, x, g.value_y, colors[i]);
    }

    let (x0, x1, y0, y1) = (g.x0, g.x1, g.y0, g.y1);
    let series: Vec<Vec<(usize, f32)>> = metrics
        .iter()
        .map(|k| d.history.iter().enumerate().filter_map(|(i, s)| s.get(k).map(|v| (i, *v))).collect())
        .collect();
    let units: Vec<Option<Unit>> = metrics.iter().map(|k| metric(k).map(|m| m.unit)).collect();

    // Which series go in which lane, and the unit each lane's axis is labelled in.
    let mut groups: Vec<(Option<Unit>, Vec<usize>)> = Vec::new();
    if lanes {
        for (i, u) in units.iter().enumerate() {
            match groups.iter_mut().find(|(gu, _)| u.is_some() && gu == u) {
                Some((_, members)) => members.push(i),
                None => groups.push((*u, vec![i])),
            }
        }
    } else {
        let shared = units.first().copied().flatten().filter(|u| units.iter().all(|x| *x == Some(*u)));
        groups.push((shared, (0..metrics.len()).collect()));
    }

    let k = groups.len().max(1) as f32;
    let gap = if groups.len() > 1 { 12.0 } else { 0.0 };
    let lane_h = ((y1 - y0) - gap * (k - 1.0)) / k;
    let len = d.history.len();
    let (px, pad) = (g.axis_px, g.axis_px * 0.25);

    // Per-series value range for each lane: the lane's unit auto-fit, or each metric's nominal range.
    let lane_ranges: Vec<Vec<(f32, f32)>> = groups
        .iter()
        .map(|(unit, members)| match unit {
            Some(u) => {
                let r = fit_range(*u, members.iter().flat_map(|&i| series[i].iter().map(|p| p.1)));
                vec![r; members.len()]
            }
            None => members.iter().map(|&i| metric(&metrics[i]).map_or((0.0, 100.0), |m| (m.min, m.max))).collect(),
        })
        .collect();
    let lpx = px.min(lane_h * 0.42).max(11.0);
    let two_labels = lane_h >= lpx * 2.2;
    let lane_labels: Vec<Vec<String>> = groups
        .iter()
        .zip(&lane_ranges)
        .map(|((unit, _), r)| match (unit, r.first()) {
            (Some(u), Some(&(lo, hi))) if two_labels => vec![u.format(hi), u.format(lo)],
            (Some(u), Some(&(lo, hi))) => vec![format!("{}–{}", u.format(lo), u.format(hi))],
            _ => Vec::new(),
        })
        .collect();
    // Lanes keep their axis labels in a left margin so they never sit on the data.
    let gutter = if lanes {
        let width = |t: &str| fonts.regular.as_ref().map_or(t.chars().count() as f32 * lpx * 0.6, |f| text_width(f, t, lpx));
        lane_labels.iter().flatten().map(|t| width(t)).fold(0.0, f32::max) + pad * 2.0
    } else {
        0.0
    };
    let px0 = x0 + gutter;

    for (lane, (_, members)) in groups.iter().enumerate() {
        let ly0 = y0 + lane as f32 * (lane_h + gap);
        let ly1 = ly0 + lane_h;
        let ranges = &lane_ranges[lane];
        let to_xy = |(i, v): (usize, f32), (lo, hi): (f32, f32)| {
            // Newest sample at the right edge; the plot fills in from the right over HISTORY_LEN s.
            let age = (len - 1 - i) as f32;
            let f = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
            (x1 - age / (HISTORY_LEN - 1) as f32 * (x1 - px0), ly1 - f * (ly1 - ly0))
        };
        let grid: &[f32] = if lane_h >= 60.0 { &[ly0, (ly0 + ly1) / 2.0, ly1] } else { &[ly0, ly1] };
        for &gy in grid {
            bar(img, px0, gy, x1 - px0, 1.0, 0.0, muted, muted.scale(0.4));
        }
        let pts: Vec<Vec<(f32, f32)>> =
            members.iter().zip(ranges).map(|(&i, r)| series[i].iter().map(|p| to_xy(*p, *r)).collect()).collect();
        if let Some(first) = pts.first() {
            fill_under(img, first, ly1, colors[members[0]], 0.15);
        }
        // Draw each lane's first series last so it sits on top.
        for (p, &i) in pts.iter().zip(members).rev() {
            polyline(img, p, colors[i], 3.0);
        }

        // Axis labels: in the margin with lanes, otherwise inside the plot so a larger plot
        // doesn't push them off the round panel.
        let labels = &lane_labels[lane];
        let font = fonts.regular.as_ref();
        let (top, bottom) = (ly0 + lpx * 0.8, ly1);
        match (labels.as_slice(), lanes) {
            ([hi, lo], true) => {
                text_right(img, font, hi, lpx, px0 - pad, top, muted);
                text_right(img, font, lo, lpx, px0 - pad, bottom, muted);
            }
            ([one], true) => text_right(img, font, one, lpx, px0 - pad, (ly0 + ly1) / 2.0 + lpx * 0.35, muted),
            ([hi, lo], false) => {
                text_left(img, font, hi, px, x0 + pad, ly0 + px, muted);
                text_left(img, font, lo, px, x0 + pad, ly1 - pad, muted);
            }
            _ => {}
        }
    }
    text_right(img, fonts.regular.as_ref(), "3 min", px, x1 - pad, y1 - pad, muted);
}

/// Auto-fitted axis range for one unit's values, padded and never narrower than a
/// readable span. Empty input gives a default span from zero.
fn fit_range(unit: Unit, values: impl Iterator<Item = f32>) -> (f32, f32) {
    let (lo, hi) = values.fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
    let (min_span, pad) = match unit {
        Unit::Celsius | Unit::Percent => (10.0, 2.0),
        Unit::Rpm => (200.0, 50.0),
        Unit::Watts => (20.0, 5.0),
    };
    let (mut lo, mut hi) = if lo > hi { (0.0, min_span) } else { (lo.floor() - pad, hi.ceil() + pad) };
    if unit != Unit::Celsius {
        lo = lo.max(0.0);
    }
    if unit == Unit::Percent {
        hi = hi.min(100.0);
    }
    if hi - lo < min_span {
        hi = lo + min_span;
    }
    (lo, hi)
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
    /// Axis label size; tracks the plot so labels stay proportionate.
    axis_px: f32,
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
    // Floor keeps labels legible on the panel at small plot sizes.
    let axis_px = (20.0 * s).max(14.0);
    GraphLayout { label_px, label_y, value_px, value_y, axis_px, x0: 240.0 - width / 2.0, x1: 240.0 + width / 2.0, y0, y1 }
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
    fn cycle_follows_names_and_skips_missing() {
        let spec = LcdSpec {
            cycle: vec!["rings".into(), "nope".into(), "graph".into()],
            cycle_seconds: 10,
            ..LcdSpec::default()
        };
        let at = |s| cycle_screen(&spec, Duration::from_secs(s)).unwrap().name.clone();
        assert_eq!(at(5), "rings");
        assert_eq!(at(15), "graph");
        assert_eq!(at(25), "rings");
        let empty = LcdSpec { cycle: vec![], ..LcdSpec::default() };
        assert_eq!(cycle_screen(&empty, Duration::ZERO).unwrap().name, "coolant");
    }

}
