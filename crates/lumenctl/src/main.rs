use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lumen_ipc::{Client, Effect, FanSpec, LcdMode, Request, Status, TempSource};

/// Control lumend: lighting, fans and the AIO screen.
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
        mode: Option<Mode>,
        /// Image or GIF for `image` mode.
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        rotation: Option<u16>,
        /// Gauge color.
        #[arg(long)]
        accent: Option<String>,
        /// Layouts for `cycle` mode, comma-separated (e.g. coolant,rings,graph).
        #[arg(long, value_delimiter = ',')]
        cycle: Option<Vec<Mode>>,
        /// Seconds per layout in `cycle` mode.
        #[arg(long)]
        interval: Option<u32>,
    },
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

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Coolant,
    System,
    Rings,
    Load,
    Cooling,
    Graph,
    Clock,
    Cycle,
    Image,
    Firmware,
    Off,
}

impl From<Mode> for LcdMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Coolant => LcdMode::Coolant,
            Mode::System => LcdMode::System,
            Mode::Rings => LcdMode::Rings,
            Mode::Load => LcdMode::Load,
            Mode::Cooling => LcdMode::Cooling,
            Mode::Graph => LcdMode::Graph,
            Mode::Clock => LcdMode::Clock,
            Mode::Cycle => LcdMode::Cycle,
            Mode::Image => LcdMode::Image,
            Mode::Firmware => LcdMode::Firmware,
            Mode::Off => LcdMode::Off,
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
        Cmd::Lcd { mode, image, brightness, rotation, accent, cycle, interval } => {
            let mut lcd = client.status()?.active.lcd;
            if let Some(m) = mode {
                lcd.mode = m.into();
            }
            if let Some(path) = image {
                let abs = std::fs::canonicalize(&path).with_context(|| format!("image {path}"))?;
                lcd.image = Some(abs.to_string_lossy().into_owned());
                if mode.is_none() {
                    lcd.mode = LcdMode::Image;
                }
            }
            if lcd.mode == LcdMode::Image && lcd.image.is_none() {
                bail!("image mode needs --image PATH");
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
                let list: Vec<LcdMode> = list.into_iter().map(LcdMode::from).collect();
                if let Some(bad) = list.iter().find(|m| !m.cyclable()) {
                    bail!("{bad:?} can't be part of a cycle");
                }
                lcd.cycle = list;
            }
            if let Some(secs) = interval {
                lcd.cycle_seconds = secs.max(1);
            }
            client.request(&Request::SetLcd { lcd })?;
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
                println!("LCD  {:?}  brightness {}%  rotation {}°", lcd.mode, lcd.brightness, lcd.rotation);
                if lcd.mode == LcdMode::Cycle {
                    let list: Vec<String> = lcd.cycle.iter().map(|m| format!("{m:?}")).collect();
                    println!("     cycling every {}s: {}", lcd.cycle_seconds, list.join(" → "));
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
