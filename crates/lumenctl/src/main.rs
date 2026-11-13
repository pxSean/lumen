use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lumen_ipc::{
    Client, Effect, FanSpec, LcdSpec, METRICS, Request, SHOW_CYCLE, SHOW_IMAGE, Screen, Status, Style, TempSource, format_metric,
};

/// Control lumen-icued: lighting, fans and the AIO screen.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show devices, sensors and the active profile (default).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Switch profile, or list profiles when no name is given.
    Profile {
        name: Option<String>,
        /// Copy the active profile to NAME instead of switching to an existing one.
        #[arg(long)]
        save: bool,
        /// Delete profile NAME.
        #[arg(long, conflicts_with = "save")]
        delete: bool,
    },
    /// Set a lighting effect. TARGET: `*`, `hub`, `hub:fans`, `hub:aio`, `hub:ch3`, ...
    ///
    /// Colors: `#rrggbb`, `theme`, or `theme:<name>` (accent, red, blue, ...).
    Light {
        target: String,
        effect: EffectKind,
        /// Color for static/breathing; comma-separated list for gradient.
        #[arg(short, long)]
        color: Option<String>,
        #[arg(short, long, default_value_t = 1.0)]
        speed: f32,
        /// Temperature effect: sensor to follow.
        #[arg(long, default_value = "coolant")]
        source: Source,
        #[arg(long, default_value = "theme:blue")]
        cold: String,
        #[arg(long, default_value = "theme:red")]
        hot: String,
        #[arg(long, default_value_t = 30.0)]
        min: f32,
        #[arg(long, default_value_t = 45.0)]
        max: f32,
    },
    /// Fan/pump control. TARGET: `fans`, `pump`, or `ch<N>`.
    ///
    /// `lumenctl fan fans 45` sets a fixed duty; `lumenctl fan pump 30:60 40:85 45:100 --source coolant`
    /// sets a curve of TEMP:PERCENT points; `lumenctl fan ch3 inherit` removes a channel override.
    /// Pumps never go below 50%.
    Fan {
        target: String,
        #[arg(required = true)]
        values: Vec<String>,
        #[arg(long, default_value = "coolant")]
        source: Source,
    },
    /// Configure the AIO screen.
    Lcd {
        /// A screen name (see `lumenctl screen`), or cycle, image, firmware, off.
        show: Option<String>,
        /// Image or GIF for `image`.
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        rotation: Option<u16>,
        /// Gauge color.
        #[arg(long)]
        accent: Option<String>,
        /// Screens for `cycle`, comma-separated (e.g. coolant,rings,graph).
        #[arg(long, value_delimiter = ',')]
        cycle: Option<Vec<String>>,
        /// Seconds per screen in `cycle`.
        #[arg(long)]
        interval: Option<u32>,
        /// Image or GIF behind every screen, or `wallpaper` for the current Omarchy wallpaper.
        #[arg(long, conflicts_with = "no_background")]
        background: Option<String>,
        /// Remove the background image.
        #[arg(long)]
        no_background: bool,
        /// How much to dim the background toward the theme color, 0-100 (default 55).
        #[arg(long)]
        dim: Option<u8>,
    },
    /// List AIO screens, or create/edit one.
    ///
    /// `lumenctl screen temps --style graph --metrics cpu.temp,gpu.temp,igpu.temp`
    /// `lumenctl screen rings --outer cpu.load --inner gpu.load --footer mem.used`
    Screen {
        name: Option<String>,
        /// coolant, cooling, clock, rings, bars or graph (required when creating).
        #[arg(long)]
        style: Option<String>,
        /// Graph/bars: 1-4 metric keys, comma-separated (see `lumenctl metrics`).
        #[arg(long, value_delimiter = ',')]
        metrics: Option<Vec<String>>,
        /// Rings: outer gauge metric.
        #[arg(long)]
        outer: Option<String>,
        /// Rings: inner gauge metric.
        #[arg(long)]
        inner: Option<String>,
        /// Rings: footer metric, or `none`.
        #[arg(long)]
        footer: Option<String>,
        /// Graph: plot size, 0.6 to 1.5.
        #[arg(long)]
        scale: Option<f32>,
        /// Graph: one lane per unit (°C, %, W, rpm), each with its own axis: on or off.
        #[arg(long, value_parser = parse_on_off)]
        lanes: Option<bool>,
        #[arg(long)]
        rename: Option<String>,
        #[arg(long, conflicts_with_all = ["style", "metrics", "outer", "inner", "footer", "scale", "lanes", "rename"])]
        delete: bool,
    },
    /// List the metrics screens can show, with current values.
    Metrics,
    /// Re-read config.toml and the Omarchy theme.
    Reload,
}

#[derive(Clone, Copy, ValueEnum)]
enum EffectKind {
    Off,
    Static,
    Breathing,
    Rainbow,
    Wave,
    Gradient,
    Temperature,
    /// Remove this target's override so it inherits from a broader one.
    Inherit,
}

#[derive(Clone, Copy, ValueEnum)]
enum Source {
    Cpu,
    Gpu,
    Coolant,
    Max,
}

impl From<Source> for TempSource {
    fn from(s: Source) -> Self {
        match s {
            Source::Cpu => TempSource::Cpu,
            Source::Gpu => TempSource::Gpu,
            Source::Coolant => TempSource::Coolant,
            Source::Max => TempSource::Max,
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Exit quietly when piped into `head` and friends.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    let mut client = Client::connect()?;
    match cli.cmd.unwrap_or(Cmd::Status { json: false }) {
        Cmd::Status { json: true } => println!("{}", serde_json::to_string_pretty(&client.status()?)?),
        Cmd::Status { json: false } => print_status(&client.status()?),
        Cmd::Profile { name: None, .. } => {
            let st = client.status()?;
            for p in st.profiles {
                println!("{} {p}", if p == st.profile { "*" } else { " " });
            }
        }
        Cmd::Profile { name: Some(name), save, delete } => {
            let req = if save {
                Request::SaveProfileAs { name }
            } else if delete {
                Request::DeleteProfile { name }
            } else {
                Request::UseProfile { name }
            };
            client.request(&req)?;
        }
        Cmd::Light { target, effect, color, speed, source, cold, hot, min, max } => {
            let color_or_theme = || color.clone().unwrap_or_else(|| "theme".into());
            let effect = match effect {
                EffectKind::Inherit => {
                    client.request(&Request::ClearLighting { target })?;
                    return Ok(());
                }
                EffectKind::Off => Effect::Off,
                EffectKind::Static => Effect::Static { color: color_or_theme() },
                EffectKind::Breathing => Effect::Breathing { color: color_or_theme(), speed },
                EffectKind::Rainbow => Effect::Rainbow { speed },
                EffectKind::Wave => Effect::Wave { speed },
                EffectKind::Gradient => Effect::Gradient {
                    colors: color
                        .as_deref()
                        .unwrap_or("theme:accent,theme:magenta")
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect(),
                    speed,
                },
                EffectKind::Temperature => Effect::Temperature { source: source.into(), cold, hot, min, max },
            };
            client.request(&Request::SetLighting { target, effect })?;
        }
        Cmd::Fan { target, values, source } => {
            if values == ["inherit"] {
                client.request(&Request::ClearFan { target })?;
                return Ok(());
            }
            let fan = if let [single] = values.as_slice()
                && !single.contains(':')
            {
                FanSpec::Fixed { percent: single.trim_end_matches('%').parse().context("percent")? }
            } else {
                let points = values
                    .iter()
                    .map(|v| {
                        let (t, p) = v.split_once(':').with_context(|| format!("expected TEMP:PERCENT, got '{v}'"))?;
                        Ok((t.parse::<f32>()?, p.trim_end_matches('%').parse::<u8>()?))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                FanSpec::Curve { source: source.into(), points }
            };
            client.request(&Request::SetFan { target, fan })?;
        }
        Cmd::Lcd { show, image, brightness, rotation, accent, cycle, interval, background, no_background, dim } => {
            let mut lcd = client.status()?.active.lcd;
            if let Some(path) = image {
                let abs = std::fs::canonicalize(&path).with_context(|| format!("image {path}"))?;
                lcd.image = Some(abs.to_string_lossy().into_owned());
                if show.is_none() {
                    lcd.show = SHOW_IMAGE.into();
                }
            }
            if let Some(s) = show {
                lcd.show = s;
            }
            if lcd.show == SHOW_IMAGE && lcd.image.is_none() {
                bail!("image needs --image PATH");
            }
            if let Some(b) = brightness {
                lcd.brightness = b.min(100);
            }
            if let Some(r) = rotation {
                lcd.rotation = r;
            }
            if let Some(a) = accent {
                lcd.accent = a;
            }
            if let Some(list) = cycle {
                lcd.cycle = list;
            }
            if let Some(secs) = interval {
                lcd.cycle_seconds = secs.max(1);
            }
            if let Some(bg) = background {
                lcd.background = Some(if bg == lumen_ipc::WALLPAPER {
                    bg
                } else {
                    std::fs::canonicalize(&bg).with_context(|| format!("background {bg}"))?.to_string_lossy().into_owned()
                });
            }
            if no_background {
                lcd.background = None;
            }
            if let Some(d) = dim {
                lcd.background_dim = d.min(100);
            }
            client.request(&Request::SetLcd { lcd })?;
        }
        Cmd::Screen { name: None, .. } => {
            let st = client.status()?;
            print_screens(&st.active.lcd);
        }
        Cmd::Screen { name: Some(name), style, metrics, outer, inner, footer, scale, lanes, rename, delete } => {
            let mut lcd = client.status()?.active.lcd;
            if delete {
                let before = lcd.screens.len();
                lcd.screens.retain(|s| s.name != name);
                if lcd.screens.len() == before {
                    bail!("no screen named '{name}'");
                }
                if lcd.screens.is_empty() {
                    bail!("can't delete the last screen");
                }
                lcd.cycle.retain(|c| *c != name);
                if lcd.show == name {
                    lcd.show = lcd.screens[0].name.clone();
                }
                return client.request(&Request::SetLcd { lcd }).map(|_| ());
            }
            let idx = match lcd.screens.iter().position(|s| s.name == name) {
                Some(i) => i,
                None => {
                    let style = style.as_deref().context("new screen: --style is required")?;
                    let st = Style::with_name(style, &Style::Clock).with_context(|| format!("unknown style '{style}'"))?;
                    lcd.screens.push(Screen { name: name.clone(), style: st });
                    lcd.screens.len() - 1
                }
            };
            let screen = &mut lcd.screens[idx];
            if let Some(style) = style.as_deref().filter(|s| *s != screen.style.name()) {
                screen.style = Style::with_name(style, &screen.style).with_context(|| format!("unknown style '{style}'"))?;
            }
            match &mut screen.style {
                Style::Graph { metrics: m, scale: sc, lanes: ln } => {
                    if let Some(v) = metrics {
                        *m = v;
                    }
                    if let Some(v) = scale {
                        *sc = v.clamp(lumen_ipc::GRAPH_SCALE_MIN, lumen_ipc::GRAPH_SCALE_MAX);
                    }
                    if let Some(v) = lanes {
                        *ln = v;
                    }
                }
                Style::Bars { metrics: m } => {
                    if let Some(v) = metrics {
                        *m = v;
                    }
                }
                Style::Rings { outer: o, inner: i, footer: f } => {
                    if let Some(v) = outer {
                        *o = v;
                    }
                    if let Some(v) = inner {
                        *i = v;
                    }
                    if let Some(v) = footer {
                        *f = (v != "none").then_some(v);
                    }
                }
                _ => {
                    if metrics.is_some() || outer.is_some() || inner.is_some() || footer.is_some() || scale.is_some() || lanes.is_some() {
                        bail!("the '{}' style has no options", screen.style.name());
                    }
                }
            }
            if let Some(new) = rename {
                for c in lcd.cycle.iter_mut().filter(|c| **c == name) {
                    *c = new.clone();
                }
                if lcd.show == name {
                    lcd.show = new.clone();
                }
                lcd.screens[idx].name = new;
            }
            client.request(&Request::SetLcd { lcd })?;
        }
        Cmd::Metrics => {
            let st = client.status()?;
            for m in METRICS {
                let value = format_metric(m.key, st.metrics.get(m.key).copied());
                let value = if value == "--" { "unavailable".to_string() } else { value };
                println!("  {:<14} {:<12} {}", m.key, value, m.description);
            }
        }
        Cmd::Reload => {
            client.request(&Request::Reload)?;
        }
    }
    Ok(())
}

fn temp(t: Option<f32>) -> String {
    t.map_or("--".into(), |v| format!("{v:.1}°C"))
}

fn print_status(st: &Status) {
    println!("profile  {}   (theme: {})", st.profile, st.theme.name);
    println!(
        "temps    cpu {}  gpu {}  coolant {}{}",
        temp(st.sensors.cpu),
        temp(st.sensors.gpu),
        temp(st.sensors.coolant),
        if st.critical { "  !! CRITICAL: fans forced to 100%" } else { "" }
    );
    for d in &st.devices {
        println!();
        match &d.error {
            Some(e) => println!("{}  [{}]  error: {e}", d.name, d.id),
            None => println!("{}  [{}]", d.name, d.id),
        }
        for z in &d.zones {
            let rpm = z.rpm.map_or(String::new(), |r| format!("{r:>5} rpm"));
            let duty = z.duty.map_or(String::new(), |p| format!("{p:>3}%"));
            let t = z.temp.map_or(String::new(), |v| format!("{v:.1}°C"));
            println!("  {:<13} {:<26} {:>3} leds  {rpm:>9}  {duty:>4}  {t}", z.target, z.name, z.leds);
        }
    }
    if let Some(lcd) = &st.lcd {
        println!();
        match &lcd.error {
            Some(e) => println!("LCD  error: {e}"),
            None => {
                let lcd = &st.active.lcd;
                println!("LCD  showing {}  brightness {}%  rotation {}°", lcd.show, lcd.brightness, lcd.rotation);
                if let Some(bg) = &lcd.background {
                    println!("     background {bg} (dim {}%)", lcd.background_dim);
                }
                if lcd.show == SHOW_CYCLE {
                    println!("     cycling every {}s: {}", lcd.cycle_seconds, lcd.cycle.join(" → "));
                }
            }
        }
    }
    println!();
    println!("lighting");
    for (t, e) in &st.active.lighting {
        println!("  {t:<13} {}", describe_effect(e));
    }
    println!("fans");
    for (t, f) in &st.active.fans {
        let desc = match f {
            FanSpec::Fixed { percent } => format!("fixed {percent}%"),
            FanSpec::Curve { source, points } => format!(
                "curve on {source:?}: {}",
                points.iter().map(|(t, p)| format!("{t}°→{p}%")).collect::<Vec<_>>().join(" ")
            ),
        };
        println!("  {t:<13} {desc}");
    }
}

fn describe_style(style: &Style) -> String {
    match style {
        Style::Graph { metrics, scale, lanes } => {
            format!("graph  {}  (size {scale:.1}×{})", metrics.join(", "), if *lanes { ", lanes" } else { "" })
        }
        Style::Bars { metrics } => format!("bars   {}", metrics.join(", ")),
        Style::Rings { outer, inner, footer } => format!(
            "rings  outer {outer}, inner {inner}{}",
            footer.as_ref().map_or(String::new(), |f| format!(", footer {f}"))
        ),
        other => other.name().to_string(),
    }
}

fn print_screens(lcd: &LcdSpec) {
    for s in &lcd.screens {
        let marks = format!(
            "{}{}",
            if s.name == lcd.show { "●" } else { " " },
            if lcd.cycle.contains(&s.name) { "↻" } else { " " }
        );
        println!("{marks} {:<12} {}", s.name, describe_style(&s.style));
    }
    println!("\n● showing   ↻ in cycle (every {}s)", lcd.cycle_seconds);
}

fn describe_effect(e: &Effect) -> String {
    match e {
        Effect::Static { color } => format!("static {color}"),
        Effect::Breathing { color, speed } => format!("breathing {color} ×{speed}"),
        Effect::Gradient { colors, speed } => format!("gradient {} ×{speed}", colors.join(",")),
        Effect::Temperature { source, cold, hot, min, max } => {
            format!("temperature {source:?} {cold}@{min}° → {hot}@{max}°")
        }
        Effect::Rainbow { speed } | Effect::Wave { speed } => format!("{} ×{speed}", e.name()),
        Effect::Off => "off".into(),
    }
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => Err(format!("expected on or off, got '{s}'")),
    }
}
