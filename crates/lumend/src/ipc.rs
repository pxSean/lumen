//! Unix-socket server: one JSON request per line, one JSON response per line.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

use anyhow::{bail, ensure};
use lumen_ipc::{Request, Response};

use crate::state::{Shared, load_config};
use crate::theme::Theme;

pub fn bind() -> anyhow::Result<UnixListener> {
    let path = lumen_ipc::socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("another lumend is already listening on {}", path.display());
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    log::info!("listening on {}", path.display());
    Ok(listener)
}

pub fn serve(listener: UnixListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = client(stream, &shared) {
                log::debug!("ipc client: {e}");
            }
        });
    }
}

fn client(stream: UnixStream, shared: &Shared) -> anyhow::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle(req, shared).unwrap_or_else(|e| Response::Error { message: format!("{e:#}") }),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
    }
    Ok(())
}

fn handle(req: Request, shared: &Shared) -> anyhow::Result<Response> {
    match req {
        Request::Status => return Ok(Response::Status(Box::new(shared.status()))),
        Request::UseProfile { name } => shared.edit(|c| {
            ensure!(c.profiles.contains_key(&name), "no profile named '{name}'");
            c.active_profile = name;
            Ok(())
        })?,
        Request::SaveProfileAs { name } => shared.edit(|c| {
            ensure!(!name.trim().is_empty(), "profile name is empty");
            let current = c.profiles.get(&c.active_profile).cloned().unwrap_or_default();
            c.profiles.insert(name.clone(), current);
            c.active_profile = name;
            Ok(())
        })?,
        Request::DeleteProfile { name } => shared.edit(|c| {
            ensure!(c.active_profile != name, "cannot delete the active profile");
            ensure!(c.profiles.remove(&name).is_some(), "no profile named '{name}'");
            Ok(())
        })?,
        Request::SetLighting { target, effect } => shared.edit(|c| {
            active(c).lighting.insert(target, effect);
            Ok(())
        })?,
        Request::ClearLighting { target } => shared.edit(|c| {
            ensure!(target != "*", "the '*' fallback cannot be cleared");
            active(c).lighting.remove(&target);
            Ok(())
        })?,
        Request::SetFan { target, fan } => shared.edit(|c| {
            if let lumen_ipc::FanSpec::Curve { points, .. } = &fan {
                ensure!(!points.is_empty(), "fan curve needs at least one point");
                ensure!(
                    points.windows(2).all(|w| w[0].0 < w[1].0),
                    "fan curve temperatures must be strictly increasing"
                );
            }
            active(c).fans.insert(target, fan);
            Ok(())
        })?,
        Request::ClearFan { target } => shared.edit(|c| {
            ensure!(target != "fans" && target != "pump", "'{target}' is a group default and cannot be cleared");
            active(c).fans.remove(&target);
            Ok(())
        })?,
        Request::SetLcd { lcd } => shared.edit(|c| {
            ensure!([0, 90, 180, 270].contains(&lcd.rotation), "rotation must be 0, 90, 180 or 270");
            if let Some(path) = &lcd.image {
                ensure!(std::path::Path::new(path).is_file(), "image not found: {path}");
            }
            active(c).lcd = lcd;
            Ok(())
        })?,
        Request::Reload => {
            let cfg = load_config()?;
            *shared.config.lock().unwrap() = cfg;
            *shared.theme.write().unwrap() = Theme::load();
            shared.bump();
        }
    }
    Ok(Response::Ok)
}

fn active(c: &mut lumen_ipc::Config) -> &mut lumen_ipc::Profile {
    c.profiles.entry(c.active_profile.clone()).or_default()
}
