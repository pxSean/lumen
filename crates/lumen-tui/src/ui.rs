//! Rendering. Colors come from the active Omarchy theme as reported by lumend.

use lumen_ipc::{Effect, FanSpec, LcdMode, Status, TempSource};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Chart, Clear, Dataset, Gauge, GraphType, List, ListItem, ListState, Paragraph, Tabs,
};

use crate::app::{App, Tab, primary_color, speed_of};

pub struct Palette {
    pub fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub red: Color,
    pub sel_bg: Color,
}

fn hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Resolves `#rrggbb`, `theme`, `theme:<name>` like the daemon does.
pub fn resolve(st: Option<&Status>, c: &str) -> Color {
    if let Some(col) = hex(c) {
        return col;
    }
    let lookup = |k: &str| {
        st.and_then(|s| s.theme.colors.iter().find(|(n, _)| n == k))
            .and_then(|(_, v)| hex(v))
    };
    match c.strip_prefix("theme") {
        Some("") => lookup("keyboard").or_else(|| lookup("accent")),
        Some(k) => k.strip_prefix(':').and_then(lookup),
        None => None,
    }
    .unwrap_or(Color::White)
}

impl Palette {
    fn from(st: Option<&Status>) -> Self {
        let has = |k: &str| st.is_some_and(|s| s.theme.colors.iter().any(|(n, _)| n == k));
        let pick = |k: &str, fallback: Color| if has(k) { resolve(st, &format!("theme:{k}")) } else { fallback };
        Palette {
            fg: pick("foreground", Color::Reset),
            muted: pick("dark_foreground", Color::DarkGray),
            accent: pick("accent", Color::Cyan),
            red: pick("red", Color::Red),
            sel_bg: pick("selection", Color::DarkGray),
        }
    }
}

fn block<'a>(title: &'a str, p: &Palette, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { p.accent } else { p.muted }))
        .title(Span::styled(format!(" {title} "), Style::default().fg(p.accent).bold()))
}

fn temp(t: Option<f32>) -> String {
    t.map_or("--".into(), |v| format!("{v:.1}°"))
}

pub fn draw(f: &mut Frame, app: &App) {
    let st = app.status.as_ref();
    let p = Palette::from(st);
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    // Header: profile, theme, temps.
    let mut left = vec![Span::styled(" lumen ", Style::default().fg(p.accent).bold())];
    let mut right = Vec::new();
    if let Some(s) = st {
        left.push(Span::styled(format!(" profile: {}  theme: {}", s.profile, s.theme.name), Style::default().fg(p.muted)));
        if s.critical {
            right.push(Span::styled(" COOLANT CRITICAL: all fans 100% ", Style::default().fg(p.red).bold()));
        }
        right.extend([
            Span::styled("CPU ", Style::default().fg(p.muted)),
            Span::styled(temp(s.sensors.cpu), Style::default().fg(p.fg)),
            Span::styled("  GPU ", Style::default().fg(p.muted)),
            Span::styled(temp(s.sensors.gpu), Style::default().fg(p.fg)),
            Span::styled("  LIQ ", Style::default().fg(p.muted)),
            Span::styled(temp(s.sensors.coolant), Style::default().fg(p.accent).bold()),
            Span::raw(" "),
        ]);
    }
    f.render_widget(Paragraph::new(Line::from(left)), header);
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), header);

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| Line::from(format!(" {} {} ", i + 1, t.title())))
        .collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.tab as usize)
            .style(Style::default().fg(p.muted))
            .highlight_style(Style::default().fg(p.accent).bold().add_modifier(Modifier::REVERSED))
            .divider(""),
        tabs,
    );

    if st.is_none() {
        let msg = app.error.as_deref().unwrap_or("connecting to lumend…");
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(p.red)).block(block("lumend", &p, true)),
            body,
        );
    } else {
        match app.tab {
            Tab::Lighting => draw_lighting(f, app, &p, body),
            Tab::Fans => draw_fans(f, app, &p, body),
            Tab::Screen => draw_screen(f, app, &p, body),
            Tab::Profiles => draw_profiles(f, app, &p, body),
        }
    }

    let help = match app.tab {
        Tab::Lighting => "↑↓ target  ←→ effect  c color  # hex  +/- speed  i inherit",
        Tab::Fans => "↑↓ target  m fixed/curve  [ ] point  ←→ temp  +/- duty  a add  x del  s source  i inherit",
        Tab::Screen => "↑↓ mode  ⏎ apply  space toggle ↻  [ ] cycle secs  < > graph size  b/B backdrop  d/D dim  +/- brightness  r rotate  c accent  o image",
        Tab::Profiles => "↑↓ select  ⏎ use  n save as  d delete",
    };
    let line = match app.flash_text() {
        Some(msg) => Line::from(Span::styled(format!(" {msg}"), Style::default().fg(p.red))),
        None => Line::from(vec![
            Span::styled(format!(" {help}"), Style::default().fg(p.muted)),
            Span::styled("   tab/1-4 switch  q quit", Style::default().fg(p.muted)),
        ]),
    };
    f.render_widget(Paragraph::new(line), footer);

    if let Some(input) = &app.input {
        let area = centered(f.area(), 50, 3);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(format!("{}▏", input.buf)).block(block(input.prompt, &p, true)),
            area,
        );
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    Rect::new(area.x + (area.width - w) / 2, area.y + area.height.saturating_sub(height) / 2, w, height)
}

fn list_state(sel: usize) -> ListState {
    ListState::default().with_selected(Some(sel))
}

fn swatch(c: Color) -> Span<'static> {
    Span::styled("██", Style::default().fg(c))
}

fn effect_summary(st: Option<&Status>, e: &Effect) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    match e {
        Effect::Off => spans.push(Span::raw("  ")),
        Effect::Rainbow { .. } | Effect::Wave { .. } => {
            for c in [Color::Red, Color::Yellow, Color::Green, Color::Cyan, Color::Blue, Color::Magenta] {
                spans.push(Span::styled("▌", Style::default().fg(c)));
            }
        }
        Effect::Gradient { colors, .. } => colors.iter().for_each(|c| spans.push(swatch(resolve(st, c)))),
        Effect::Temperature { cold, hot, .. } => {
            spans.push(swatch(resolve(st, cold)));
            spans.push(Span::raw("→"));
            spans.push(swatch(resolve(st, hot)));
        }
        _ => spans.push(swatch(resolve(st, primary_color(e).unwrap_or("theme")))),
    }
    spans.push(Span::raw(format!(" {}", e.name())));
    spans
}

fn draw_lighting(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let st = app.status.as_ref();
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
    let targets = app.light_targets();
    let items: Vec<ListItem> = targets
        .iter()
        .map(|t| {
            let (e, from) = app.effective_light(t);
            let mut spans = vec![Span::styled(format!("{:<28}", t.label), Style::default().fg(p.fg))];
            let style = if from == t.key { Style::default().fg(p.fg) } else { Style::default().fg(p.muted) };
            spans.extend(effect_summary(st, &e).into_iter().map(|s| if s.content.starts_with(' ') { s.style(style) } else { s }));
            ListItem::new(Line::from(spans))
        })
        .collect();
    f.render_stateful_widget(
        List::new(items)
            .block(block("Targets", p, true))
            .highlight_style(Style::default().bg(p.sel_bg).bold()),
        list_area,
        &mut list_state(app.light_sel.min(targets.len().saturating_sub(1))),
    );

    let Some(t) = targets.get(app.light_sel) else { return };
    let (e, from) = app.effective_light(t);
    let mut lines = vec![
        Line::from(vec![Span::styled("Target   ", Style::default().fg(p.muted)), Span::raw(t.key.clone())]),
        Line::from(
            [Span::styled("Effect   ", Style::default().fg(p.muted))]
                .into_iter()
                .chain(effect_summary(st, &e))
                .collect::<Vec<_>>(),
        ),
    ];
    if from != t.key {
        lines.push(Line::from(Span::styled(
            format!("         inherited from '{from}' — any change creates an override"),
            Style::default().fg(p.muted).italic(),
        )));
    }
    if let Some(c) = primary_color(&e) {
        lines.push(Line::from(vec![
            Span::styled("Color    ", Style::default().fg(p.muted)),
            swatch(resolve(st, c)),
            Span::raw(format!(" {c}")),
        ]));
    }
    if let Some(s) = speed_of(&e) {
        lines.push(Line::from(vec![Span::styled("Speed    ", Style::default().fg(p.muted)), Span::raw(format!("×{s}"))]));
    }
    if let Effect::Temperature { source, min, max, .. } = &e {
        lines.push(Line::from(vec![
            Span::styled("Range    ", Style::default().fg(p.muted)),
            Span::raw(format!("{source:?} {min}° → {max}°  (+/- moves the hot end)")),
        ]));
    }
    f.render_widget(Paragraph::new(lines).block(block("Effect", p, false)), detail);
}

fn source_temp(st: &Status, s: TempSource) -> Option<f32> {
    match s {
        TempSource::Cpu => st.sensors.cpu,
        TempSource::Gpu => st.sensors.gpu,
        TempSource::Coolant => st.sensors.coolant,
        TempSource::Max => match (st.sensors.cpu, st.sensors.gpu) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
    }
}

fn draw_fans(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let Some(st) = app.status.as_ref() else { return };
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
    let targets = app.fan_targets();
    let items: Vec<ListItem> = targets
        .iter()
        .map(|t| {
            let rpm = if t.zones.len() == 1 { t.zones[0].rpm.map_or(String::new(), |r| format!("{r} rpm")) } else { String::new() };
            let duty = if t.zones.is_empty() {
                String::new()
            } else {
                let avg = t.zones.iter().filter_map(|z| z.duty).map(u32::from).sum::<u32>() / t.zones.len() as u32;
                format!("{avg:>3}%")
            };
            let own = app.effective_fan(t).is_some_and(|(_, k)| k == t.key);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<24}", t.label), Style::default().fg(if own { p.fg } else { p.muted })),
                Span::styled(format!("{rpm:>9} "), Style::default().fg(p.muted)),
                Span::styled(duty, Style::default().fg(p.accent)),
            ]))
        })
        .collect();
    let sel = app.fan_sel.min(targets.len().saturating_sub(1));
    f.render_stateful_widget(
        List::new(items).block(block("Fans & pump", p, true)).highlight_style(Style::default().bg(p.sel_bg).bold()),
        list_area,
        &mut list_state(sel),
    );

    let Some(t) = targets.get(sel) else { return };
    let Some((spec, from)) = app.effective_fan(t) else { return };
    let inherited = if from != t.key { format!(" (inherited from '{from}')") } else { String::new() };
    match spec {
        FanSpec::Fixed { percent } => {
            let title = format!("Fixed{inherited}");
            let [g, _] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(detail);
            f.render_widget(
                Gauge::default()
                    .block(block(&title, p, false))
                    .gauge_style(Style::default().fg(p.accent))
                    .percent(percent as u16)
                    .label(format!("{percent}%")),
                g,
            );
        }
        FanSpec::Curve { source, points } => {
            let pts: Vec<(f64, f64)> = points.iter().map(|&(t, d)| (t as f64, d as f64)).collect();
            let sel_pt = app.point_sel.min(points.len().saturating_sub(1));
            let selected: Vec<(f64, f64)> = pts.get(sel_pt).copied().into_iter().collect();
            let now_t = source_temp(st, source);
            let now_duty = t.zones.iter().filter_map(|z| z.duty).max();
            let now: Vec<(f64, f64)> = now_t.zip(now_duty).map(|(t, d)| (t as f64, d as f64)).into_iter().collect();
            let xmin = pts.first().map_or(20.0, |p| p.0).min(now_t.unwrap_or(100.0) as f64) - 5.0;
            let xmax = pts.last().map_or(60.0, |p| p.0).max(now_t.unwrap_or(0.0) as f64) + 5.0;
            let mut line = vec![(xmin, pts.first().map_or(0.0, |p| p.1))];
            line.extend(&pts);
            line.push((xmax, pts.last().map_or(100.0, |p| p.1)));
            let datasets = vec![
                Dataset::default().graph_type(GraphType::Line).marker(Marker::Braille).style(Style::default().fg(p.accent)).data(&line),
                Dataset::default().marker(Marker::Block).style(Style::default().fg(p.fg)).data(&pts),
                Dataset::default().marker(Marker::Block).style(Style::default().fg(p.red)).data(&selected),
                Dataset::default().marker(Marker::Dot).style(Style::default().fg(Color::Green)).data(&now),
            ];
            let title = format!(
                "Curve on {source:?}{inherited}  ·  point {}/{}: {}° → {}%",
                sel_pt + 1,
                points.len(),
                points.get(sel_pt).map_or(0.0, |p| p.0),
                points.get(sel_pt).map_or(0, |p| p.1)
            );
            let axis_style = Style::default().fg(p.muted);
            f.render_widget(
                Chart::new(datasets)
                    .block(block(&title, p, false))
                    .x_axis(
                        Axis::default()
                            .title("°C")
                            .style(axis_style)
                            .bounds([xmin, xmax])
                            .labels([format!("{xmin:.0}"), format!("{:.0}", (xmin + xmax) / 2.0), format!("{xmax:.0}")]),
                    )
                    .y_axis(Axis::default().title("%").style(axis_style).bounds([0.0, 100.0]).labels(["0", "50", "100"])),
                detail,
            );
        }
    }
}

fn draw_screen(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let Some(st) = app.status.as_ref() else { return };
    let lcd = &st.active.lcd;
    let [list_area, detail] = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
    let describe = |m: LcdMode| match m {
        LcdMode::Coolant => "Coolant gauge",
        LcdMode::System => "CPU / GPU / coolant bars",
        LcdMode::Rings => "CPU & GPU rings",
        LcdMode::Load => "CPU & GPU load",
        LcdMode::Cooling => "Pump & fans",
        LcdMode::Graph => "Temperature graph",
        LcdMode::Clock => "Clock",
        LcdMode::Cycle => "Cycle layouts",
        LcdMode::Image => "Image / GIF",
        LcdMode::Firmware => "Corsair firmware screen",
        LcdMode::Off => "Off",
    };
    let items: Vec<ListItem> = LcdMode::ALL
        .iter()
        .map(|m| {
            let active = *m == lcd.mode;
            let in_cycle = lcd.cycle.contains(m);
            ListItem::new(Line::from(vec![
                Span::styled(if active { "● " } else { "  " }, Style::default().fg(p.accent)),
                Span::styled(format!("{:<26}", describe(*m)), Style::default().fg(if active { p.fg } else { p.muted })),
                Span::styled(if in_cycle { "↻" } else { "" }, Style::default().fg(p.muted)),
            ]))
        })
        .collect();
    f.render_stateful_widget(
        List::new(items).block(block("AIO screen", p, true)).highlight_style(Style::default().bg(p.sel_bg).bold()),
        list_area,
        &mut list_state(app.lcd_sel),
    );

    let [g, info] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(detail);
    f.render_widget(
        Gauge::default()
            .block(block("Brightness", p, false))
            .gauge_style(Style::default().fg(p.accent))
            .percent(lcd.brightness as u16),
        g,
    );
    let conn = match &st.lcd {
        Some(l) if l.connected => format!("connected ({})", l.serial),
        Some(l) => format!("error: {}", l.error.as_deref().unwrap_or("?")),
        None => "not detected".into(),
    };
    let lines = vec![
        Line::from(vec![Span::styled("Panel     ", Style::default().fg(p.muted)), Span::raw(conn)]),
        Line::from(vec![Span::styled("Rotation  ", Style::default().fg(p.muted)), Span::raw(format!("{}°", lcd.rotation))]),
        Line::from(vec![
            Span::styled("Cycle     ", Style::default().fg(p.muted)),
            Span::raw(format!("every {}s (↻ marks layouts in the cycle)", lcd.cycle_seconds)),
        ]),
        Line::from(vec![
            Span::styled("Backdrop  ", Style::default().fg(p.muted)),
            Span::raw(match &lcd.background {
                Some(bg) => format!("{bg}  (dim {}%, d/D to change)", lcd.background_dim),
                None => "theme color  (b to set an image or 'wallpaper')".into(),
            }),
        ]),
        Line::from(vec![
            Span::styled("Graph     ", Style::default().fg(p.muted)),
            Span::raw(format!("plot size {:.1}×  (< > to change)", lcd.graph_scale)),
        ]),
        Line::from(vec![
            Span::styled("Accent    ", Style::default().fg(p.muted)),
            swatch(resolve(Some(st), &lcd.accent)),
            Span::raw(format!(" {}", lcd.accent)),
        ]),
        Line::from(vec![
            Span::styled("Image     ", Style::default().fg(p.muted)),
            Span::raw(lcd.image.clone().unwrap_or_else(|| "—".into())),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(block("Settings", p, false)), info);
}

fn draw_profiles(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let Some(st) = app.status.as_ref() else { return };
    let items: Vec<ListItem> = st
        .profiles
        .iter()
        .map(|name| {
            let active = *name == st.profile;
            ListItem::new(Line::from(vec![
                Span::styled(if active { "● " } else { "  " }, Style::default().fg(p.accent)),
                Span::styled(name.clone(), Style::default().fg(if active { p.fg } else { p.muted })),
            ]))
        })
        .collect();
    f.render_stateful_widget(
        List::new(items).block(block("Profiles", p, true)).highlight_style(Style::default().bg(p.sel_bg).bold()),
        area,
        &mut list_state(app.profile_sel.min(st.profiles.len().saturating_sub(1))),
    );
}
