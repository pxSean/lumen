//! CPU temperature from hwmon and utilization from /proc/stat; GPU readings from NVML
//! (NVIDIA) or amdgpu sysfs; memory from /proc/meminfo. Published as metrics keyed
//! as in `lumen_ipc::METRICS`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

pub struct SystemSensors {
    cpu: Option<PathBuf>,
    /// Primary (discrete when present) GPU: `gpu.*` metrics.
    gpu: Option<Gpu>,
    /// Integrated graphics alongside a discrete card: `igpu.*` metrics.
    igpu: Option<Gpu>,
    /// (busy, total) jiffies from the previous /proc/stat sample.
    prev_cpu: Option<(u64, u64)>,
}

enum Gpu {
    /// NVIDIA's proprietary driver exposes no hwmon; NVML is loaded at runtime
    /// (libnvidia-ml.so), so machines without it simply skip this source.
    Nvidia { nvml: Nvml, index: u32 },
    Amd { hwmon: PathBuf, temp: PathBuf, busy: Option<PathBuf> },
}

impl SystemSensors {
    pub fn discover() -> Self {
        let mut s = Self { cpu: None, gpu: None, igpu: None, prev_cpu: None };
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
        let mut amd = discover_amd();
        s.gpu = match discover_nvidia() {
            Some(nv) => Some(nv),
            None => (!amd.is_empty()).then(|| amd.remove(0)),
        };
        // Whatever amdgpu is left over is the CPU's integrated graphics.
        s.igpu = amd.pop();
        let gpu = match &s.gpu {
            Some(Gpu::Nvidia { nvml, index }) => nvml
                .device_by_index(*index)
                .and_then(|d| d.name())
                .map_or_else(|_| "NVIDIA (NVML)".into(), |n| format!("{n} (NVML)")),
            Some(Gpu::Amd { temp, .. }) => format!("amdgpu {}", temp.display()),
            None => "none".into(),
        };
        let igpu = match &s.igpu {
            Some(Gpu::Amd { temp, .. }) => format!("amdgpu {}", temp.display()),
            _ => "none".into(),
        };
        log::info!("cpu sensor: {:?}, gpu: {gpu}, igpu: {igpu}", s.cpu);
        s
    }

    pub fn cpu(&self) -> Option<f32> {
        self.cpu.as_deref().and_then(read_milli)
    }

    /// Every system metric that can be read right now (call once per second; CPU load
    /// is computed from the previous call).
    pub fn metrics(&mut self) -> BTreeMap<String, f32> {
        let mut m = BTreeMap::new();
        let mut put = |k: &str, v: Option<f32>| {
            if let Some(v) = v {
                m.insert(k.to_string(), v);
            }
        };
        put("cpu.temp", self.cpu());
        put("cpu.load", self.cpu_load());
        put("mem.used", mem_used());
        for (prefix, gpu) in [("gpu", &self.gpu), ("igpu", &self.igpu)] {
            let Some(g) = gpu else { continue };
            put(&format!("{prefix}.temp"), g.temp());
            put(&format!("{prefix}.load"), g.load());
            if prefix == "gpu" {
                put("gpu.power", g.power());
                put("gpu.vram", g.vram());
            }
        }
        m
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

impl Gpu {
    fn temp(&self) -> Option<f32> {
        match self {
            Gpu::Nvidia { nvml, index } => {
                Some(nvml.device_by_index(*index).ok()?.temperature(TemperatureSensor::Gpu).ok()? as f32)
            }
            Gpu::Amd { temp, .. } => read_milli(temp),
        }
    }

    fn load(&self) -> Option<f32> {
        match self {
            Gpu::Nvidia { nvml, index } => Some(nvml.device_by_index(*index).ok()?.utilization_rates().ok()?.gpu as f32),
            Gpu::Amd { busy, .. } => fs::read_to_string(busy.as_ref()?).ok()?.trim().parse().ok(),
        }
    }

    fn power(&self) -> Option<f32> {
        match self {
            Gpu::Nvidia { nvml, index } => Some(nvml.device_by_index(*index).ok()?.power_usage().ok()? as f32 / 1000.0),
            // power1_average (older kernels) or power1_input, in microwatts.
            Gpu::Amd { hwmon, .. } => ["power1_average", "power1_input"]
                .iter()
                .find_map(|f| fs::read_to_string(hwmon.join(f)).ok())
                .and_then(|v| v.trim().parse::<f32>().ok())
                .map(|uw| uw / 1_000_000.0),
        }
    }

    fn vram(&self) -> Option<f32> {
        let (used, total) = match self {
            Gpu::Nvidia { nvml, index } => {
                let mem = nvml.device_by_index(*index).ok()?.memory_info().ok()?;
                (mem.used as f32, mem.total as f32)
            }
            Gpu::Amd { hwmon, .. } => {
                let read = |f: &str| fs::read_to_string(hwmon.join("device").join(f)).ok()?.trim().parse::<f32>().ok();
                (read("mem_info_vram_used")?, read("mem_info_vram_total")?)
            }
        };
        (total > 0.0).then(|| used / total * 100.0)
    }
}

fn mem_used() -> Option<f32> {
    let info = fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| {
        info.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<f32>().ok())
    };
    let (total, available) = (field("MemTotal:")?, field("MemAvailable:")?);
    (total > 0.0).then(|| (total - available) / total * 100.0)
}

fn discover_nvidia() -> Option<Gpu> {
    let nvml = Nvml::init().map_err(|e| log::debug!("NVML unavailable: {e}")).ok()?;
    (nvml.device_count().ok()? > 0).then_some(Gpu::Nvidia { nvml, index: 0 })
}

/// Every amdgpu, most VRAM first, so a discrete card sorts ahead of the CPU's
/// integrated graphics (which reports only a small carve-out).
fn discover_amd() -> Vec<Gpu> {
    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else { return Vec::new() };
    let mut found: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|dir| fs::read_to_string(dir.join("name")).is_ok_and(|n| n.trim() == "amdgpu"))
        .map(|dir| {
            let vram = fs::read_to_string(dir.join("device/mem_info_vram_total"))
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0);
            (vram, dir)
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
        .into_iter()
        .map(|(_, dir)| {
            // temp1 is edge; junction (temp2) tracks load better when present.
            let junction = dir.join("temp2_input");
            let busy = dir.join("device/gpu_busy_percent");
            Gpu::Amd {
                temp: if junction.exists() { junction } else { dir.join("temp1_input") },
                busy: busy.exists().then_some(busy),
                hwmon: dir,
            }
        })
        .collect()
}

fn read_milli(p: &Path) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}
