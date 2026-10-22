//! Reads the active Omarchy theme and resolves color references against it.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use lumen_hid::Rgb;
use lumen_ipc::ThemeInfo;

#[derive(Debug, Clone, Default)]
pub struct Theme {
    pub name: String,
    pub colors: BTreeMap<String, Rgb>,
    stamp: Option<SystemTime>,
}

fn state_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/state/omarchy/current")
}

/// Changes whenever `omarchy theme set` runs.
fn stamp() -> Option<SystemTime> {
    let dir = state_dir();
    let name = fs::metadata(dir.join("theme.name")).and_then(|m| m.modified()).ok();
    let colors = fs::metadata(dir.join("theme/colors.toml")).and_then(|m| m.modified()).ok();
    name.max(colors)
}

impl Theme {
    pub fn load() -> Self {
        let dir = state_dir();
        let mut theme = Theme {
            name: fs::read_to_string(dir.join("theme.name"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".into()),
            colors: BTreeMap::new(),
            stamp: stamp(),
        };
        if let Ok(text) = fs::read_to_string(dir.join("theme/colors.toml"))
            && let Ok(table) = text.parse::<toml::Table>()
        {
            for (k, v) in table {
                if let Some(c) = v.as_str().and_then(parse_hex) {
                    theme.colors.insert(k, c);
                }
            }
        }
        if let Some(c) = fs::read_to_string(dir.join("theme/keyboard.rgb"))
            .ok()
            .and_then(|s| parse_hex(s.trim()))
        {
            theme.colors.insert("keyboard".into(), c);
        }
        theme
    }

    pub fn changed_on_disk(&self) -> bool {
        stamp() != self.stamp
    }

    /// `#rrggbb`, `theme` (keyboard.rgb, else accent), or `theme:<key>`. Unknown → white.
    pub fn resolve(&self, color: &str) -> Rgb {
        let fallback = Rgb(255, 255, 255);
        if let Some(c) = parse_hex(color) {
            return c;
        }
        match color.strip_prefix("theme") {
            Some("") => self
                .colors
                .get("keyboard")
                .or_else(|| self.colors.get("accent"))
                .copied()
                .unwrap_or(fallback),
            Some(key) => key
                .strip_prefix(':')
                .and_then(|k| self.colors.get(k))
                .copied()
                .unwrap_or(fallback),
            None => fallback,
        }
    }

    pub fn info(&self) -> ThemeInfo {
        ThemeInfo {
            name: self.name.clone(),
            colors: self
                .colors
                .iter()
                .map(|(k, c)| (k.clone(), format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)))
                .collect(),
        }
    }
}

pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}
