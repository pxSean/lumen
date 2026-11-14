//! CPU temperature from hwmon and utilization from /proc/stat; GPU readings from NVML
//! (NVIDIA's driver) or the GPU driver's hwmon (amdgpu, Intel xe/i915, nouveau);
//! memory from /proc/meminfo. Published as metrics keyed as in `lumen_ipc::METRICS`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    /// Boxed: Nvml is about 12 KB of function pointers.
    Nvidia { nvml: Box<Nvml>, index: u32 },
    Hwmon(HwmonGpu),
}

/// A GPU read through its driver's hwmon directory. What each driver exposes:
/// - amdgpu: temperature, busy %, power, VRAM (discrete cards and APUs).
/// - xe (Intel): temperature and an energy counter. Discrete cards only.
/// - i915 (Intel): an energy counter. Discrete cards only.
/// - nouveau: temperature, and power on some cards.
///
/// Intel drivers have no busy % in sysfs, and Intel integrated graphics have no
/// hwmon at all, so those readings are simply absent.
struct HwmonGpu {
    driver: String,
    temp: Option<PathBuf>,
    busy: Option<PathBuf>,
    /// Instantaneous or averaged power, in microwatts.
    power: Option<PathBuf>,
    /// Cumulative energy in microjoules, for drivers with no power reading; power is
    /// the change between samples. Holds the previous sample.
    energy: Option<EnergyCounter>,
    /// (used, total) VRAM in bytes.
    vram: Option<(PathBuf, PathBuf)>,
    vram_total: u64,
}

struct EnergyCounter {
    path: PathBuf,
    prev: Cell<Option<(u64, Instant)>>,
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
        let (gpu, igpu) = pick_gpus(discover_nvidia(), discover_hwmon_gpus(Path::new("/sys/class/hwmon")));
        s.gpu = gpu;
        s.igpu = igpu;
        let describe = |g: &Option<Gpu>| match g {
            Some(Gpu::Nvidia { nvml, index }) => nvml
                .device_by_index(*index)
                .and_then(|d| d.name())
                .map_or_else(|_| "NVIDIA (NVML)".into(), |n| format!("{n} (NVML)")),
            Some(Gpu::Hwmon(h)) => h.describe(),
            None => "none".into(),
        };
        let (gpu, igpu) = (describe(&s.gpu), describe(&s.igpu));
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
            Gpu::Hwmon(h) => read_milli(h.temp.as_ref()?),
        }
    }

    fn load(&self) -> Option<f32> {
        match self {
            Gpu::Nvidia { nvml, index } => Some(nvml.device_by_index(*index).ok()?.utilization_rates().ok()?.gpu as f32),
            Gpu::Hwmon(h) => fs::read_to_string(h.busy.as_ref()?).ok()?.trim().parse().ok(),
        }
    }

    fn power(&self) -> Option<f32> {
        match self {
            Gpu::Nvidia { nvml, index } => Some(nvml.device_by_index(*index).ok()?.power_usage().ok()? as f32 / 1000.0),
            Gpu::Hwmon(h) => h.power(),
        }
    }

    fn vram(&self) -> Option<f32> {
        let (used, total) = match self {
            Gpu::Nvidia { nvml, index } => {
                let mem = nvml.device_by_index(*index).ok()?.memory_info().ok()?;
                (mem.used as f32, mem.total as f32)
            }
            Gpu::Hwmon(h) => {
                let (used, total) = h.vram.as_ref()?;
                let read = |p: &Path| fs::read_to_string(p).ok()?.trim().parse::<f32>().ok();
                (read(used)?, read(total)?)
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
    (nvml.device_count().ok()? > 0).then_some(Gpu::Nvidia { nvml: Box::new(nvml), index: 0 })
}

impl HwmonGpu {
    fn describe(&self) -> String {
        let temp = self.temp.as_ref().map_or("no temperature".into(), |t| t.display().to_string());
        format!("{} {temp}", self.driver)
    }

    /// Intel's hwmon is only registered for discrete cards, and nouveau only drives
    /// discrete cards. amdgpu covers both, so it goes by VRAM.
    fn known_discrete(&self) -> bool {
        matches!(self.driver.as_str(), "xe" | "i915" | "nouveau")
    }

    fn power(&self) -> Option<f32> {
        if let Some(p) = &self.power {
            return read_u64(p).map(|uw| uw as f32 / 1_000_000.0);
        }
        let e = self.energy.as_ref()?;
        let now = (read_u64(&e.path)?, Instant::now());
        let (e0, t0) = e.prev.replace(Some(now))?;
        let dt = now.1.duration_since(t0).as_secs_f32();
        // A counter that went backwards (wrapped or reset) gives no reading this time.
        (dt > 0.0 && now.0 >= e0).then(|| (now.0 - e0) as f32 / 1_000_000.0 / dt)
    }
}

/// The primary GPU and the integrated one. NVML wins when present. Otherwise the
/// primary is a card known to be discrete, else the amdgpu with the most VRAM. The
/// integrated GPU is an amdgpu left over (Intel integrated graphics have no hwmon).
fn pick_gpus(nvidia: Option<Gpu>, mut hwmon: Vec<HwmonGpu>) -> (Option<Gpu>, Option<Gpu>) {
    hwmon.sort_by_key(|g| std::cmp::Reverse((g.known_discrete(), g.vram_total)));
    let mut rest = hwmon.into_iter();
    let gpu = nvidia.or_else(|| rest.next().map(Gpu::Hwmon));
    let igpu = rest.filter(|g| g.driver == "amdgpu").last().map(Gpu::Hwmon);
    (gpu, igpu)
}

/// Every GPU driver hwmon directory under `root` (normally /sys/class/hwmon).
fn discover_hwmon_gpus(root: &Path) -> Vec<HwmonGpu> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    dirs.into_iter().filter_map(|dir| hwmon_gpu(&dir)).collect()
}

fn hwmon_gpu(dir: &Path) -> Option<HwmonGpu> {
    let driver = fs::read_to_string(dir.join("name")).ok()?.trim().to_string();
    if !matches!(driver.as_str(), "amdgpu" | "xe" | "i915" | "nouveau") {
        return None;
    }
    let existing = |f: &str| Some(dir.join(f)).filter(|p| p.exists());
    let device = |f: &str| Some(dir.join("device").join(f)).filter(|p| p.exists());
    let vram = device("mem_info_vram_used").zip(device("mem_info_vram_total"));
    let vram_total = vram.as_ref().and_then(|(_, t)| read_u64(t)).unwrap_or(0);
    // Energy: xe labels energy1 "card" and energy2 "pkg"; the card total comes first.
    let energy = ["energy1_input", "energy2_input"].iter().find_map(|f| existing(f));
    Some(HwmonGpu {
        temp: pick_temp(dir),
        busy: device("gpu_busy_percent"),
        // power1_average on older amdgpu kernels, power1_input on newer ones and nouveau.
        power: ["power1_average", "power1_input"].iter().find_map(|f| existing(f)),
        energy: energy.map(|path| EnergyCounter { path, prev: Cell::new(None) }),
        vram,
        vram_total,
        driver,
    })
}

/// The temperature that best tracks load: amdgpu's "junction", xe's "pkg", else
/// "edge" or the lowest-numbered input.
fn pick_temp(dir: &Path) -> Option<PathBuf> {
    let mut temps: Vec<(u32, String, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n: u32 = name.strip_prefix("temp")?.strip_suffix("_input")?.parse().ok()?;
            let label = fs::read_to_string(dir.join(format!("temp{n}_label"))).unwrap_or_default();
            Some((n, label.trim().to_lowercase(), e.path()))
        })
        .collect();
    temps.sort_by_key(|t| t.0);
    ["junction", "pkg", "edge"]
        .iter()
        .find_map(|want| temps.iter().position(|t| t.1 == *want))
        .or((!temps.is_empty()).then_some(0))
        .map(|i| temps.swap_remove(i).2)
}

fn read_u64(p: &Path) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn read_milli(p: &Path) -> Option<f32> {
    fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|v| v / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway /sys/class/hwmon tree: each entry is (dir, [(file, contents)]).
    fn tree(name: &str, dirs: &[(&str, &[(&str, &str)])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("lumen-hwmon-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (dir, files) in dirs {
            for (file, contents) in *files {
                let p = root.join(dir).join(file);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, contents).unwrap();
            }
        }
        root
    }

    fn hw(g: &Option<Gpu>) -> &HwmonGpu {
        match g {
            Some(Gpu::Hwmon(h)) => h,
            _ => panic!("expected a hwmon GPU"),
        }
    }

    const APU: &[(&str, &str)] = &[
        ("name", "amdgpu\n"),
        ("temp1_input", "41000\n"),
        ("temp1_label", "edge\n"),
        ("power1_input", "12000000\n"),
        ("device/gpu_busy_percent", "3\n"),
        ("device/mem_info_vram_total", "536870912\n"),
        ("device/mem_info_vram_used", "268435456\n"),
    ];

    #[test]
    fn amd_discrete_card_beats_apu_and_reads_everything() {
        let root = tree("amd", &[
            ("hwmon1", APU),
            ("hwmon2", &[
                ("name", "amdgpu\n"),
                ("temp1_input", "50000\n"),
                ("temp1_label", "edge\n"),
                ("temp2_input", "62000\n"),
                ("temp2_label", "junction\n"),
                ("power1_average", "150000000\n"),
                ("device/gpu_busy_percent", "87\n"),
                ("device/mem_info_vram_total", "17179869184\n"),
                ("device/mem_info_vram_used", "4294967296\n"),
            ]),
            ("hwmon3", &[("name", "k10temp\n"), ("temp1_input", "45000\n")]),
        ]);
        let (gpu, igpu) = pick_gpus(None, discover_hwmon_gpus(&root));
        let g = gpu.unwrap();
        assert_eq!(g.temp(), Some(62.0), "junction preferred over edge");
        assert_eq!(g.load(), Some(87.0));
        assert_eq!(g.power(), Some(150.0));
        assert_eq!(g.vram(), Some(25.0));
        let i = igpu.unwrap();
        assert_eq!(i.temp(), Some(41.0));
        assert_eq!(i.vram(), Some(50.0));
    }

    #[test]
    fn lone_apu_is_the_primary_gpu() {
        let root = tree("apu", &[("hwmon0", APU)]);
        let (gpu, igpu) = pick_gpus(None, discover_hwmon_gpus(&root));
        assert_eq!(gpu.unwrap().load(), Some(3.0));
        assert!(igpu.is_none());
    }

    #[test]
    fn intel_xe_card_is_primary_over_apu_with_power_from_energy() {
        let root = tree("xe", &[
            ("hwmon0", APU),
            ("hwmon4", &[
                ("name", "xe\n"),
                ("temp2_input", "55000\n"),
                ("temp2_label", "pkg\n"),
                ("temp3_input", "60000\n"),
                ("temp3_label", "vram\n"),
                ("energy1_input", "1000000\n"),
                ("energy1_label", "card\n"),
            ]),
        ]);
        let (gpu, igpu) = pick_gpus(None, discover_hwmon_gpus(&root));
        assert_eq!(hw(&gpu).driver, "xe");
        assert_eq!(hw(&igpu).driver, "amdgpu");
        let g = gpu.unwrap();
        assert_eq!(g.temp(), Some(55.0), "pkg preferred");
        assert_eq!(g.load(), None, "Intel has no busy %");
        assert_eq!(g.vram(), None);
        // First energy sample only primes the counter.
        assert_eq!(g.power(), None);
        std::thread::sleep(std::time::Duration::from_millis(100));
        fs::write(root.join("hwmon4/energy1_input"), "11000000\n").unwrap(); // +10 J
        let w = g.power().unwrap();
        assert!((20.0..=110.0).contains(&w), "10 J over ~0.1 s, got {w} W");
        // A counter reset gives no reading rather than a huge or negative one.
        fs::write(root.join("hwmon4/energy1_input"), "5\n").unwrap();
        assert_eq!(g.power(), None);
    }

    #[test]
    fn intel_i915_card_has_power_but_no_temperature() {
        let root = tree("i915", &[("hwmon2", &[("name", "i915\n"), ("energy1_input", "0\n")])]);
        let (gpu, _) = pick_gpus(None, discover_hwmon_gpus(&root));
        assert_eq!(hw(&gpu).driver, "i915");
        assert_eq!(gpu.unwrap().temp(), None);
    }

    #[test]
    fn nouveau_reads_temperature_and_power() {
        let root = tree("nouveau", &[(
            "hwmon1",
            &[("name", "nouveau\n"), ("temp1_input", "48000\n"), ("power1_input", "35000000\n")],
        )]);
        let (gpu, igpu) = pick_gpus(None, discover_hwmon_gpus(&root));
        let g = gpu.unwrap();
        assert_eq!(g.temp(), Some(48.0));
        assert_eq!(g.power(), Some(35.0));
        assert!(igpu.is_none());
    }

    #[test]
    fn no_gpu_drivers_means_no_gpu() {
        let root = tree("none", &[("hwmon0", &[("name", "nvme\n"), ("temp1_input", "30000\n")])]);
        let (gpu, igpu) = pick_gpus(None, discover_hwmon_gpus(&root));
        assert!(gpu.is_none() && igpu.is_none());
    }
}
