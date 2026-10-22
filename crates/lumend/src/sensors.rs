//! CPU and GPU temperatures from hwmon, utilization from /proc/stat and amdgpu.

use std::fs;
use std::path::PathBuf;

pub struct SystemSensors {
    cpu: Option<PathBuf>,
    gpu: Option<PathBuf>,
    gpu_busy: Option<PathBuf>,
    /// (busy, total) jiffies from the previous /proc/stat sample.
    prev_cpu: Option<(u64, u64)>,
}

impl SystemSensors {
    pub fn discover() -> Self {
        let mut s = Self { cpu: None, gpu: None, gpu_busy: None, prev_cpu: None };
        s.gpu_busy = fs::read_dir("/sys/class/drm")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("device/gpu_busy_percent"))
            .find(|p| p.exists());
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
        log::info!("cpu sensor: {:?}, gpu sensor: {:?}, gpu load: {:?}", s.cpu, s.gpu, s.gpu_busy);
        s
    }

    pub fn cpu(&self) -> Option<f32> {
        self.cpu.as_ref().and_then(read_milli)
    }

    pub fn gpu(&self) -> Option<f32> {
        self.gpu.as_ref().and_then(read_milli)
    }

    /// Utilization since the previous call, 0-100%. `None` on the first call.
    pub fn cpu_load(&mut self) -> Option<f32> {
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
        // user nice system idle iowait irq softirq steal
        let idle = fields.get(3)? + fields.get(4).unwrap_or(&0);
        let total: u64 = fields.iter().take(8).sum();
        let busy = total - idle;
        let prev = self.prev_cpu.replace((busy, total))?;
        let dt = total.saturating_sub(prev.1);
        (dt > 0).then(|| busy.saturating_sub(prev.0) as f32 / dt as f32 * 100.0)
    }

    pub fn gpu_load(&self) -> Option<f32> {
        fs::read_to_string(self.gpu_busy.as_ref()?).ok()?.trim().parse().ok()
    }
}

fn read_milli(p: &PathBuf) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}
