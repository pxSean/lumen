//! CPU and GPU temperatures from hwmon.

use std::fs;
use std::path::PathBuf;

pub struct SystemSensors {
    cpu: Option<PathBuf>,
    gpu: Option<PathBuf>,
}

impl SystemSensors {
    pub fn discover() -> Self {
        let mut s = Self { cpu: None, gpu: None };
        let Ok(entries) = fs::read_dir("/sys/class/hwmon") else { return s };
        for e in entries.flatten() {
            let dir = e.path();
            let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
            match name.trim() {
                // temp1 is Tctl on k10temp and the package sensor on coretemp.
                "k10temp" | "coretemp" | "zenpower" if s.cpu.is_none() => s.cpu = Some(dir.join("temp1_input")),
                // temp1 is edge; prefer junction (temp2) when present, it tracks load better.
                "amdgpu" if s.gpu.is_none() => {
                    let junction = dir.join("temp2_input");
                    s.gpu = Some(if junction.exists() { junction } else { dir.join("temp1_input") });
                }
                _ => {}
            }
        }
        log::info!("cpu sensor: {:?}, gpu sensor: {:?}", s.cpu, s.gpu);
        s
    }

    pub fn cpu(&self) -> Option<f32> {
        self.cpu.as_ref().and_then(read_milli)
    }

    pub fn gpu(&self) -> Option<f32> {
        self.gpu.as_ref().and_then(read_milli)
    }
}

fn read_milli(p: &PathBuf) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}
