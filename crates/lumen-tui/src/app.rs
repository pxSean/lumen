//! TUI state and key handling. Every edit is sent to lumend immediately.

use std::time::{Duration, Instant};

use lumen_ipc::{Client, Effect, FanSpec, LcdMode, Request, Status, TempSource, ZoneStatus};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Lighting,
    Fans,
    Screen,
    Profiles,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Lighting, Tab::Fans, Tab::Screen, Tab::Profiles];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Lighting => "Lighting",
            Tab::Fans => "Fans",
            Tab::Screen => "Screen",
            Tab::Profiles => "Profiles",
        }
    }
}

pub enum InputKind {
    HexColor,
    ImagePath,
    NewProfile,
}

pub struct Input {
    pub kind: InputKind,
    pub prompt: &'static str,
    pub buf: String,
}

pub struct App {
    pub client: Option<Client>,
    pub status: Option<Status>,
    pub error: Option<String>,
    pub flash: Option<(String, Instant)>,
    pub tab: Tab,
    pub light_sel: usize,
    pub fan_sel: usize,
    pub point_sel: usize,
    pub lcd_sel: usize,
    pub profile_sel: usize,
    pub input: Option<Input>,
    pub quit: bool,
}

pub const EFFECTS: [&str; 7] = ["off", "static", "breathing", "rainbow", "wave", "gradient", "temperature"];
pub const PALETTE: [&str; 10] = [
    "theme",
    "theme:accent",
    "theme:red",
    "theme:yellow",
    "theme:green",
    "theme:cyan",
    "theme:blue",
    "theme:magenta",
    "theme:foreground",
    "#ffffff",
];

/// A lighting target and the chain of keys it inherits from, most specific first.
pub struct LightTarget {
    pub key: String,
    pub label: String,
    pub chain: Vec<String>,
}

pub struct FanTarget {
    pub key: String,
    pub label: String,
    pub chain: Vec<String>,
    pub zones: Vec<ZoneStatus>,
}

fn group_of(kind: &str) -> &'static str {
    match kind {
        "aio" => "hub:aio",
        "pump" => "hub:pump",
        "fan" => "hub:fans",
        _ => "hub:other",
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            client: None,
            status: None,
            error: None,
            flash: None,
            tab: Tab::Lighting,
            light_sel: 0,
            fan_sel: 0,
            point_sel: 0,
            lcd_sel: 0,
            profile_sel: 0,
            input: None,
            quit: false,
        }
    }

    pub fn refresh(&mut self) {
        if self.client.is_none() {
            match Client::connect() {
                Ok(c) => self.client = Some(c),
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        match self.client.as_mut().unwrap().status() {
            Ok(s) => {
                self.status = Some(s);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.client = None;
            }
        }
    }

    fn send(&mut self, req: Request) {
        let Some(client) = self.client.as_mut() else { return };
        match client.request(&req) {
            Ok(_) => self.refresh(),
            Err(e) => self.flash(format!("{e:#}")),
        }
    }

    pub fn flash(&mut self, msg: String) {
        self.flash = Some((msg, Instant::now()));
    }

    pub fn flash_text(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(4))
            .map(|(m, _)| m.as_str())
    }

    pub fn light_targets(&self) -> Vec<LightTarget> {
        let mut out = vec![LightTarget { key: "*".into(), label: "Everything".into(), chain: vec!["*".into()] }];
        let Some(st) = &self.status else { return out };
        let t = |key: &str, label: String, chain: &[&str]| LightTarget {
            key: key.into(),
            label,
            chain: chain.iter().map(|s| s.to_string()).collect(),
        };
        for d in st.devices.iter().filter(|d| d.connected) {
            let id = d.id.as_str();
            out.push(t(id, d.name.split(" (").next().unwrap_or(&d.name).to_string(), &[id, "*"]));
            if id == "hub" {
                out.push(t("hub:fans", "  All fans".into(), &["hub:fans", "hub", "*"]));
                out.push(t("hub:aio", "  AIO pump head".into(), &["hub:aio", "hub", "*"]));
            }
            for z in d.zones.iter().filter(|z| z.leds > 0) {
                let short = z.target.split_once(':').map_or(z.target.as_str(), |(_, s)| s);
                let label = if id == "hub" {
                    format!("    {short} {}", z.name.trim_start_matches("iCUE LINK "))
                } else {
                    format!("  {}", z.name)
                };
                let mut chain = vec![z.target.as_str()];
                if id == "hub" {
                    chain.push(group_of(&z.kind));
                }
                chain.extend([id, "*"]);
                out.push(t(&z.target, label, &chain));
            }
        }
        out
    }

    pub fn fan_targets(&self) -> Vec<FanTarget> {
        let zones: Vec<ZoneStatus> = self
            .status
            .iter()
            .flat_map(|s| &s.devices)
            .flat_map(|d| d.zones.iter().cloned())
            .filter(|z| z.duty.is_some())
            .collect();
        let is_pump = |z: &ZoneStatus| z.kind == "aio" || z.kind == "pump";
        let mut out = vec![
            FanTarget {
                key: "fans".into(),
                label: "All fans".into(),
                chain: vec!["fans".into()],
                zones: zones.iter().filter(|z| !is_pump(z)).cloned().collect(),
            },
            FanTarget {
                key: "pump".into(),
                label: "Pump".into(),
                chain: vec!["pump".into()],
                zones: zones.iter().filter(|z| is_pump(z)).cloned().collect(),
            },
        ];
        for z in &zones {
            let ch = z.target.trim_start_matches("hub:").to_string();
            out.push(FanTarget {
                key: ch.clone(),
                label: format!("  {ch} {}", z.name.trim_start_matches("iCUE LINK ")),
                chain: vec![ch, if is_pump(z) { "pump" } else { "fans" }.into()],
                zones: vec![z.clone()],
            });
        }
        out
    }

    /// The effect that applies to a target, and the key it comes from.
    pub fn effective_light(&self, t: &LightTarget) -> (Effect, String) {
        let lighting = self.status.as_ref().map(|s| &s.active.lighting);
        t.chain
            .iter()
            .find_map(|k| lighting.and_then(|l| l.get(k)).map(|e| (e.clone(), k.clone())))
            .unwrap_or((Effect::Static { color: "theme".into() }, "*".into()))
    }

    pub fn effective_fan(&self, t: &FanTarget) -> Option<(FanSpec, String)> {
        let fans = &self.status.as_ref()?.active.fans;
        t.chain.iter().find_map(|k| fans.get(k).map(|f| (f.clone(), k.clone())))
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if self.input.is_some() {
            return self.on_input_key(key);
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('1') => self.tab = Tab::Lighting,
            KeyCode::Char('2') => self.tab = Tab::Fans,
            KeyCode::Char('3') => self.tab = Tab::Screen,
            KeyCode::Char('4') => self.tab = Tab::Profiles,
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab as usize + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab as usize + Tab::ALL.len() - 1) % Tab::ALL.len()],
            _ => match self.tab {
                Tab::Lighting => self.lighting_key(key),
                Tab::Fans => self.fans_key(key),
                Tab::Screen => self.screen_key(key),
                Tab::Profiles => self.profiles_key(key),
            },
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        let input = self.input.as_mut().unwrap();
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Backspace => {
                input.buf.pop();
            }
            KeyCode::Char(c) => input.buf.push(c),
            KeyCode::Enter => {
                let Input { kind, buf, .. } = self.input.take().unwrap();
                let value = buf.trim().to_string();
                if value.is_empty() {
                    return;
                }
                match kind {
                    InputKind::HexColor => {
                        let hex = if value.starts_with('#') { value } else { format!("#{value}") };
                        if hex.len() != 7 || u32::from_str_radix(&hex[1..], 16).is_err() {
                            return self.flash(format!("not a #rrggbb color: {hex}"));
                        }
                        self.set_color(hex);
                    }
                    InputKind::ImagePath => {
                        let path = shellexpand_home(&value);
                        match std::fs::canonicalize(&path) {
                            Ok(abs) => {
                                if let Some(mut lcd) = self.status.as_ref().map(|s| s.active.lcd.clone()) {
                                    lcd.image = Some(abs.to_string_lossy().into_owned());
                                    lcd.mode = LcdMode::Image;
                                    self.send(Request::SetLcd { lcd });
                                }
                            }
                            Err(e) => self.flash(format!("{path}: {e}")),
                        }
                    }
                    InputKind::NewProfile => self.send(Request::SaveProfileAs { name: value }),
                }
            }
            _ => {}
        }
    }

    // ---- Lighting -------------------------------------------------------------------------

    fn selected_light(&self) -> Option<LightTarget> {
        let mut t = self.light_targets();
        (self.light_sel < t.len()).then(|| t.swap_remove(self.light_sel))
    }

    fn set_light(&mut self, target: String, effect: Effect) {
        self.send(Request::SetLighting { target, effect });
    }

    fn lighting_key(&mut self, key: KeyEvent) {
        let n = self.light_targets().len();
        let Some(t) = self.selected_light() else {
            self.light_sel = 0;
            return;
        };
        let (effect, _) = self.effective_light(&t);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.light_sel = (self.light_sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.light_sel = (self.light_sel + 1) % n,
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') => {
                self.set_light(t.key, cycle_effect(&effect, 1));
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('E') => {
                self.set_light(t.key, cycle_effect(&effect, EFFECTS.len() - 1));
            }
            KeyCode::Char('c') => {
                let next = next_color(primary_color(&effect).unwrap_or("theme"));
                self.set_color(next.into());
            }
            KeyCode::Char('#') | KeyCode::Char('C') => {
                self.input = Some(Input { kind: InputKind::HexColor, prompt: "Color (#rrggbb)", buf: String::new() });
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.set_light(t.key, with_speed(effect, 0.25)),
            KeyCode::Char('-') => self.set_light(t.key, with_speed(effect, -0.25)),
            KeyCode::Char('i') | KeyCode::Delete => {
                if t.key == "*" {
                    self.flash("'Everything' is the fallback; it can't inherit".into());
                } else {
                    self.send(Request::ClearLighting { target: t.key });
                }
            }
            _ => {}
        }
    }

    fn set_color(&mut self, color: String) {
        let Some(t) = self.selected_light() else { return };
        let (effect, _) = self.effective_light(&t);
        let effect = match effect {
            Effect::Breathing { speed, .. } => Effect::Breathing { color, speed },
            Effect::Gradient { mut colors, speed } => {
                // Cycling the color edits the first stop.
                if colors.is_empty() {
                    colors.push(color);
                } else {
                    colors[0] = color;
                }
                Effect::Gradient { colors, speed }
            }
            Effect::Temperature { source, cold, min, max, .. } => Effect::Temperature { source, cold, hot: color, min, max },
            _ => Effect::Static { color },
        };
        self.set_light(t.key, effect);
    }

    // ---- Fans -----------------------------------------------------------------------------

    fn fans_key(&mut self, key: KeyEvent) {
        let targets = self.fan_targets();
        let n = targets.len();
        if n == 0 {
            return;
        }
        self.fan_sel = self.fan_sel.min(n - 1);
        let t = &targets[self.fan_sel];
        let spec = self.effective_fan(t).map(|(s, _)| s);
        let key_name = t.key.clone();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.fan_sel = (self.fan_sel + n - 1) % n;
                self.point_sel = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.fan_sel = (self.fan_sel + 1) % n;
                self.point_sel = 0;
            }
            KeyCode::Char('m') => {
                let fan = match spec {
                    Some(FanSpec::Curve { .. }) | None => FanSpec::Fixed { percent: 50 },
                    Some(FanSpec::Fixed { .. }) => FanSpec::Curve {
                        source: TempSource::Coolant,
                        points: vec![(30.0, 40), (38.0, 60), (45.0, 100)],
                    },
                };
                self.send(Request::SetFan { target: key_name, fan });
            }
            KeyCode::Char('i') | KeyCode::Delete => {
                if key_name.starts_with("ch") {
                    self.send(Request::ClearFan { target: key_name });
                } else {
                    self.flash("group defaults can't inherit; per-channel overrides can".into());
                }
            }
            _ => match spec {
                Some(FanSpec::Fixed { percent }) => {
                    let delta: i16 = match key.code {
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') | KeyCode::Char('=') => 5,
                        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => -5,
                        _ => return,
                    };
                    let percent = (percent as i16 + delta).clamp(0, 100) as u8;
                    self.send(Request::SetFan { target: key_name, fan: FanSpec::Fixed { percent } });
                }
                Some(FanSpec::Curve { source, mut points }) => {
                    let np = points.len();
                    self.point_sel = self.point_sel.min(np.saturating_sub(1));
                    let i = self.point_sel;
                    match key.code {
                        KeyCode::Char(']') | KeyCode::Char('n') => self.point_sel = (i + 1) % np.max(1),
                        KeyCode::Char('[') | KeyCode::Char('p') => self.point_sel = (i + np.max(1) - 1) % np.max(1),
                        KeyCode::Char('s') => {
                            let source = match source {
                                TempSource::Coolant => TempSource::Cpu,
                                TempSource::Cpu => TempSource::Gpu,
                                TempSource::Gpu => TempSource::Max,
                                TempSource::Max => TempSource::Coolant,
                            };
                            self.send(Request::SetFan { target: key_name, fan: FanSpec::Curve { source, points } });
                        }
                        KeyCode::Char('a') => {
                            let (t, p) = points.get(i).copied().unwrap_or((30.0, 50));
                            let next_t = points.get(i + 1).map_or(t + 5.0, |n| (t + n.0) / 2.0).round();
                            if points.iter().any(|q| q.0 == next_t) {
                                return self.flash("no room for a point there".into());
                            }
                            points.insert(i + 1, (next_t, p));
                            self.point_sel = i + 1;
                            self.send(Request::SetFan { target: key_name, fan: FanSpec::Curve { source, points } });
                        }
                        KeyCode::Char('x') if np > 1 => {
                            points.remove(i);
                            self.point_sel = i.saturating_sub(1);
                            self.send(Request::SetFan { target: key_name, fan: FanSpec::Curve { source, points } });
                        }
                        KeyCode::Char('K') | KeyCode::Char('J') | KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') => {
                            let d: i16 = if matches!(key.code, KeyCode::Char('K') | KeyCode::Char('+') | KeyCode::Char('=')) { 5 } else { -5 };
                            if let Some(pt) = points.get_mut(i) {
                                pt.1 = (pt.1 as i16 + d).clamp(0, 100) as u8;
                                self.send(Request::SetFan { target: key_name, fan: FanSpec::Curve { source, points } });
                            }
                        }
                        KeyCode::Char('L') | KeyCode::Char('H') | KeyCode::Right | KeyCode::Left => {
                            let d = if matches!(key.code, KeyCode::Char('L') | KeyCode::Right) { 1.0 } else { -1.0 };
                            let lo = if i > 0 { points[i - 1].0 + 1.0 } else { 0.0 };
                            let hi = points.get(i + 1).map_or(100.0, |n| n.0 - 1.0);
                            if let Some(pt) = points.get_mut(i) {
                                pt.0 = (pt.0 + d).clamp(lo, hi);
                                self.send(Request::SetFan { target: key_name, fan: FanSpec::Curve { source, points } });
                            }
                        }
                        _ => {}
                    }
                }
                None => {}
            },
        }
    }

    // ---- Screen ---------------------------------------------------------------------------

    fn screen_key(&mut self, key: KeyEvent) {
        let Some(mut lcd) = self.status.as_ref().map(|s| s.active.lcd.clone()) else { return };
        let n = LcdMode::ALL.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.lcd_sel = (self.lcd_sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.lcd_sel = (self.lcd_sel + 1) % n,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let mode = LcdMode::ALL[self.lcd_sel];
                if mode == LcdMode::Image && lcd.image.is_none() {
                    self.input = Some(Input { kind: InputKind::ImagePath, prompt: "Image or GIF path", buf: String::new() });
                    return;
                }
                lcd.mode = mode;
                self.send(Request::SetLcd { lcd });
            }
            KeyCode::Char('o') => {
                self.input = Some(Input {
                    kind: InputKind::ImagePath,
                    prompt: "Image or GIF path",
                    buf: lcd.image.clone().unwrap_or_default(),
                });
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Right | KeyCode::Char('l') => {
                lcd.brightness = (lcd.brightness + 10).min(100);
                self.send(Request::SetLcd { lcd });
            }
            KeyCode::Char('-') | KeyCode::Left | KeyCode::Char('h') => {
                lcd.brightness = lcd.brightness.saturating_sub(10);
                self.send(Request::SetLcd { lcd });
            }
            KeyCode::Char('r') => {
                lcd.rotation = (lcd.rotation + 90) % 360;
                self.send(Request::SetLcd { lcd });
            }
            KeyCode::Char('c') => {
                lcd.accent = next_color(&lcd.accent).into();
                self.send(Request::SetLcd { lcd });
            }
            _ => {}
        }
    }

    // ---- Profiles -------------------------------------------------------------------------

    fn profiles_key(&mut self, key: KeyEvent) {
        let profiles = self.status.as_ref().map(|s| s.profiles.clone()).unwrap_or_default();
        let n = profiles.len().max(1);
        self.profile_sel = self.profile_sel.min(n - 1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.profile_sel = (self.profile_sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.profile_sel = (self.profile_sel + 1) % n,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(name) = profiles.get(self.profile_sel) {
                    self.send(Request::UseProfile { name: name.clone() });
                }
            }
            KeyCode::Char('n') => {
                self.input = Some(Input { kind: InputKind::NewProfile, prompt: "Save current profile as", buf: String::new() });
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(name) = profiles.get(self.profile_sel) {
                    self.send(Request::DeleteProfile { name: name.clone() });
                }
            }
            _ => {}
        }
    }
}

fn shellexpand_home(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}

pub fn primary_color(e: &Effect) -> Option<&str> {
    match e {
        Effect::Static { color } | Effect::Breathing { color, .. } => Some(color),
        Effect::Gradient { colors, .. } => colors.first().map(String::as_str),
        Effect::Temperature { hot, .. } => Some(hot),
        _ => None,
    }
}

fn next_color(current: &str) -> &'static str {
    let i = PALETTE.iter().position(|c| *c == current).map_or(0, |i| i + 1);
    PALETTE[i % PALETTE.len()]
}

pub fn speed_of(e: &Effect) -> Option<f32> {
    match e {
        Effect::Breathing { speed, .. } | Effect::Rainbow { speed } | Effect::Wave { speed } | Effect::Gradient { speed, .. } => {
            Some(*speed)
        }
        _ => None,
    }
}

fn with_speed(e: Effect, delta: f32) -> Effect {
    let adj = |s: f32| ((s + delta) * 100.0).round().clamp(0.0, 1000.0) / 100.0;
    match e {
        Effect::Breathing { color, speed } => Effect::Breathing { color, speed: adj(speed).max(0.25) },
        Effect::Rainbow { speed } => Effect::Rainbow { speed: adj(speed).max(0.25) },
        Effect::Wave { speed } => Effect::Wave { speed: adj(speed).max(0.25) },
        Effect::Gradient { colors, speed } => Effect::Gradient { colors, speed: adj(speed) },
        Effect::Temperature { source, cold, hot, min, max } => {
            // +/- shifts the hot end of the range.
            Effect::Temperature { source, cold, hot, min, max: (max + delta * 4.0).max(min + 1.0) }
        }
        other => other,
    }
}

fn cycle_effect(e: &Effect, step: usize) -> Effect {
    let i = EFFECTS.iter().position(|n| *n == e.name()).unwrap_or(0);
    let color = primary_color(e).unwrap_or("theme").to_string();
    let speed = speed_of(e).unwrap_or(1.0).max(0.25);
    match EFFECTS[(i + step) % EFFECTS.len()] {
        "off" => Effect::Off,
        "static" => Effect::Static { color },
        "breathing" => Effect::Breathing { color, speed },
        "rainbow" => Effect::Rainbow { speed },
        "wave" => Effect::Wave { speed },
        "gradient" => Effect::Gradient { colors: vec![color, "theme:magenta".into()], speed: 0.5 },
        _ => Effect::Temperature {
            source: TempSource::Coolant,
            cold: "theme:blue".into(),
            hot: "theme:red".into(),
            min: 30.0,
            max: 45.0,
        },
    }
}
