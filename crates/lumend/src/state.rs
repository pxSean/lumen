//! State shared between the device workers and the IPC server.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::Instant;

use anyhow::Context;
use lumen_ipc::{Config, DeviceStatus, Effect, FanSpec, LcdStatus, Profile, Sensors, Status};

use crate::theme::Theme;

pub struct Shared {
    pub config: Mutex<Config>,
    pub theme: RwLock<Theme>,
    pub runtime: Mutex<Runtime>,
    pub running: AtomicBool,
    /// Bumped on every config or theme change so workers know to re-read.
    pub generation: AtomicU64,
    pub started: Instant,
}

/// One sample per second, oldest first.
pub const HISTORY_LEN: usize = 180;

#[derive(Default)]
pub struct Runtime {
    pub sensors: Sensors,
    pub history: std::collections::VecDeque<Sensors>,
    pub critical: bool,
    pub devices: Vec<DeviceStatus>,
    pub lcd: Option<LcdStatus>,
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("lumen/config.toml")
}

pub fn load_config() -> anyhow::Result<Config> {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            if !cfg.profiles.contains_key(&cfg.active_profile) {
                cfg.profiles.entry(cfg.active_profile.clone()).or_default();
            }
            Ok(cfg)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = Config::default();
            save_config(&cfg)?;
            log::info!("wrote default config to {}", path.display());
            Ok(cfg)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_config(cfg: &Config) -> anyhow::Result<()> {
    let path = config_path();
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, toml::to_string(cfg)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

impl Shared {
    pub fn new(config: Config) -> Self {
        Self {
            config: Mutex::new(config),
            theme: RwLock::new(Theme::load()),
            runtime: Mutex::new(Runtime::default()),
            running: AtomicBool::new(true),
            generation: AtomicU64::new(1),
            started: Instant::now(),
        }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn runtime(&self) -> MutexGuard<'_, Runtime> {
        self.runtime.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn active_profile(&self) -> Profile {
        let cfg = self.config.lock().unwrap();
        cfg.profiles.get(&cfg.active_profile).cloned().unwrap_or_default()
    }

    /// Applies `f` to the config, persists it, and notifies workers.
    pub fn edit<T>(&self, f: impl FnOnce(&mut Config) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut cfg = self.config.lock().unwrap();
        let mut next = cfg.clone();
        let out = f(&mut next)?;
        save_config(&next)?;
        *cfg = next;
        drop(cfg);
        self.bump();
        Ok(out)
    }

    pub fn status(&self) -> Status {
        let (profile, profiles, active) = {
            let cfg = self.config.lock().unwrap();
            (
                cfg.active_profile.clone(),
                cfg.profiles.keys().cloned().collect(),
                cfg.profiles.get(&cfg.active_profile).cloned().unwrap_or_default(),
            )
        };
        let rt = self.runtime();
        Status {
            profile,
            profiles,
            sensors: rt.sensors,
            critical: rt.critical,
            theme: self.theme.read().unwrap().info(),
            devices: rt.devices.clone(),
            lcd: rt.lcd.clone(),
            active,
        }
    }

    pub fn update_device(&self, status: DeviceStatus) {
        let mut rt = self.runtime();
        match rt.devices.iter_mut().find(|d| d.id == status.id) {
            Some(d) => *d = status,
            None => rt.devices.push(status),
        }
    }
}

/// Most-specific match first; falls back to the theme color.
pub fn lighting_for(profile: &Profile, keys: &[&str]) -> Effect {
    keys.iter()
        .chain(std::iter::once(&"*"))
        .find_map(|k| profile.lighting.get(*k).cloned())
        .unwrap_or(Effect::Static { color: "theme".into() })
}

pub fn fan_for(profile: &Profile, keys: &[&str]) -> Option<FanSpec> {
    keys.iter().find_map(|k| profile.fans.get(*k).cloned())
}
