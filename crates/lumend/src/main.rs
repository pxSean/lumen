mod effects;
mod hub;
mod ipc;
mod lcd;
mod lightdev;
mod sensors;
mod state;
mod theme;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::sensors::SystemSensors;
use crate::state::Shared;
use crate::theme::Theme;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();

    if let Some(i) = std::env::args().position(|a| a == "--preview") {
        let dir = std::env::args().nth(i + 1).unwrap_or_else(|| "lcd-preview".into());
        return lcd::preview(std::path::Path::new(&dir));
    }

    let shared = Arc::new(Shared::new(state::load_config()?));
    let listener = ipc::bind()?;

    let term = Arc::new(AtomicBool::new(false));
    let hup = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, term.clone())?;
    signal_hook::flag::register(SIGINT, term.clone())?;
    signal_hook::flag::register(SIGHUP, hup.clone())?;

    {
        let shared = shared.clone();
        std::thread::Builder::new().name("ipc".into()).spawn(move || ipc::serve(listener, shared))?;
    }
    let workers = [
        spawn("hub", &shared, hub::run)?,
        spawn("lcd", &shared, lcd::run)?,
        spawn("k70", &shared, lightdev::run::<lumen_hid::k70::K70>)?,
        spawn("g502", &shared, lightdev::run::<lumen_hid::g502::G502>)?,
    ];

    let mut sensors = SystemSensors::discover();
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    let mut last_theme_check = Instant::now();
    while !term.load(Ordering::Relaxed) {
        if last_poll.elapsed() >= Duration::from_secs(1) {
            last_poll = Instant::now();
            let (cpu, gpu) = (sensors.cpu(), sensors.gpu());
            let (cpu_load, gpu_load) = (sensors.cpu_load(), sensors.gpu_load());
            let mut rt = shared.runtime();
            rt.sensors.cpu = cpu;
            rt.sensors.gpu = gpu;
            rt.sensors.cpu_load = cpu_load;
            rt.sensors.gpu_load = gpu_load;
            let sample = rt.sensors;
            rt.history.push_back(sample);
            if rt.history.len() > state::HISTORY_LEN {
                rt.history.pop_front();
            }
        }
        let theme_changed = last_theme_check.elapsed() >= Duration::from_secs(2)
            && shared.theme.read().unwrap().changed_on_disk();
        if theme_changed || hup.swap(false, Ordering::Relaxed) {
            last_theme_check = Instant::now();
            let theme = Theme::load();
            log::info!("theme: {}", theme.name);
            *shared.theme.write().unwrap() = theme;
            shared.bump();
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    log::info!("shutting down");
    shared.running.store(false, Ordering::Relaxed);
    for w in workers {
        let _ = w.join();
    }
    let _ = std::fs::remove_file(lumen_ipc::socket_path());
    Ok(())
}

fn spawn(
    name: &str,
    shared: &Arc<Shared>,
    f: fn(Arc<Shared>),
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let shared = shared.clone();
    std::thread::Builder::new().name(name.into()).spawn(move || f(shared))
}
