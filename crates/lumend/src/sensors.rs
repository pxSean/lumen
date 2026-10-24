//! CPU temperature from hwmon and utilization from /proc/stat; GPU temperature and
//! utilization from NVML (NVIDIA) or amdgpu sysfs.

use std::fs;
use std::path::{Path, PathBuf};

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

pub struct SystemSensors {
    cpu: Option<PathBuf>,
    gpu: Option<Gpu>,
    /// (busy, total) jiffies from the previous /proc/stat sample.
    prev_cpu: Option<(u64, u64)>,
}

enum Gpu {
    /// NVIDIA's proprietary driver exposes no hwmon; NVML is loaded at runtime
    /// (libnvidia-ml.so), so machines without it simply skip this source.
    Nvidia { nvml: Nvml, index: u32 },
    Amd { temp: PathBuf, busy: Option<PathBuf> },
}

impl SystemSensors {
    pub fn discover() -> Self {
        let mut s = Self { cpu: None, gpu: None, prev_cpu: None };
        if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
            for e in entries.flatten() {
                let dir = e.path();
                let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
                // temp1 is Tctl on k10temp and the package sensor on coretemp.
                if matches!(name.trim(), "k10temp" | "coretemp" | "zenpower") && s.cpu.is_none() {
                    s.cpu = Some(dir.join("temp1_input"));
                }
            }
        }
        s.gpu = discover_nvidia().or_else(discover_amd);
        let gpu = match &s.gpu {
            Some(Gpu::Nvidia { nvml, index }) => nvml
                .device_by_index(*index)
                .and_then(|d| d.name())
                .map_or_else(|_| "NVIDIA (NVML)".into(), |n| format!("{n} (NVML)")),
            Some(Gpu::Amd { temp, .. }) => format!("amdgpu {}", temp.display()),
            None => "none".into(),
        };
        log::info!("cpu sensor: {:?}, gpu: {gpu}", s.cpu);
        s
    }

    pub fn cpu(&self) -> Option<f32> {
        self.cpu.as_deref().and_then(read_milli)
    }

    pub fn gpu(&self) -> Option<f32> {
        match self.gpu.as_ref()? {
            Gpu::Nvidia { nvml, index } => {
                let t = nvml.device_by_index(*index).ok()?.temperature(TemperatureSensor::Gpu).ok()?;
                Some(t as f32)
            }
            Gpu::Amd { temp, .. } => read_milli(temp),
        }
    }

    pub fn gpu_load(&self) -> Option<f32> {
        match self.gpu.as_ref()? {
            Gpu::Nvidia { nvml, index } => {
                Some(nvml.device_by_index(*index).ok()?.utilization_rates().ok()?.gpu as f32)
            }
            Gpu::Amd { busy, .. } => fs::read_to_string(busy.as_ref()?).ok()?.trim().parse().ok(),
        }
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
}

fn discover_nvidia() -> Option<Gpu> {
    let nvml = Nvml::init().map_err(|e| log::debug!("NVML unavailable: {e}")).ok()?;
    (nvml.device_count().ok()? > 0).then_some(Gpu::Nvidia { nvml, index: 0 })
}

/// Prefers the amdgpu with the most VRAM, so a discrete card wins over the CPU's
/// integrated graphics (which reports only a small carve-out).
fn discover_amd() -> Option<Gpu> {
    let entries = fs::read_dir("/sys/class/hwmon").ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| fs::read_to_string(dir.join("name")).is_ok_and(|n| n.trim() == "amdgpu"))
        .max_by_key(|dir| {
            fs::read_to_string(dir.join("device/mem_info_vram_total"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0)
        })
        .map(|dir| {
            // temp1 is edge; junction (temp2) tracks load better when present.
            let junction = dir.join("temp2_input");
            let busy = dir.join("device/gpu_busy_percent");
            Gpu::Amd {
                temp: if junction.exists() { junction } else { dir.join("temp1_input") },
                busy: busy.exists().then_some(busy),
            }
        })
}

fn read_milli(p: &Path) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}
