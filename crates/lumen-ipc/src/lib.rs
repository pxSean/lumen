//! Shared types between `lumend`, `lumenctl` and `lumen-tui`.
//!
//! The daemon listens on `$XDG_RUNTIME_DIR/lumen.sock`. Each request is one line of JSON
//! ([`Request`]), answered by one line of JSON ([`Response`]).

pub mod model;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

pub use model::*;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(dir).join("lumen.sock")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Switch to an existing profile.
    UseProfile { name: String },
    /// Copy the active profile under a new name and switch to it.
    SaveProfileAs { name: String },
    DeleteProfile { name: String },
    /// Set the lighting effect for a target (see [`model::Profile::lighting`]).
    SetLighting { target: String, effect: Effect },
    /// Remove a target's lighting override so it inherits from a broader target.
    ClearLighting { target: String },
    SetFan { target: String, fan: FanSpec },
    /// Remove a per-channel fan override so it follows its group (`fans`/`pump`) again.
    ClearFan { target: String },
    SetLcd { lcd: LcdSpec },
    /// Re-read config.toml and the Omarchy theme.
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status(Box<Status>),
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub profile: String,
    pub profiles: Vec<String>,
    pub sensors: Sensors,
    /// True while the coolant safety override is forcing all fans and pumps to 100%.
    pub critical: bool,
    pub theme: ThemeInfo,
    pub devices: Vec<DeviceStatus>,
    pub lcd: Option<LcdStatus>,
    /// The active profile, fully resolved.
    pub active: Profile,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sensors {
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
    pub coolant: Option<f32>,
    /// Utilization, 0-100%.
    #[serde(default)]
    pub cpu_load: Option<f32>,
    #[serde(default)]
    pub gpu_load: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThemeInfo {
    pub name: String,
    /// Named colors from the theme's colors.toml plus `keyboard` from keyboard.rgb, as #rrggbb.
    pub colors: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatus {
    /// Lighting/fan target prefix, e.g. `hub`, `k70`, `g502`, `mobo`.
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub error: Option<String>,
    pub zones: Vec<ZoneStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneStatus {
    /// Full target key, e.g. `hub:ch3`.
    pub target: String,
    pub name: String,
    pub kind: String,
    pub leds: u16,
    pub rpm: Option<u16>,
    pub temp: Option<f32>,
    /// Duty cycle most recently sent, 0-100.
    pub duty: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdStatus {
    pub connected: bool,
    pub serial: String,
    pub error: Option<String>,
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect() -> anyhow::Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("cannot connect to lumend at {} (is it running?)", path.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    pub fn call(&mut self, req: &Request) -> anyhow::Result<Response> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp)? == 0 {
            bail!("lumend closed the connection");
        }
        Ok(serde_json::from_str(&resp)?)
    }

    /// Like [`call`](Self::call) but turns `Response::Error` into `Err`.
    pub fn request(&mut self, req: &Request) -> anyhow::Result<Response> {
        match self.call(req)? {
            Response::Error { message } => bail!(message),
            r => Ok(r),
        }
    }

    pub fn status(&mut self) -> anyhow::Result<Status> {
        match self.request(&Request::Status)? {
            Response::Status(s) => Ok(*s),
            r => bail!("unexpected response: {r:?}"),
        }
    }
}
