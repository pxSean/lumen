//! Exercises the G502 and K70 LUX drivers for a few seconds each, then restores onboard lighting.

use std::time::Duration;

use lumen_hid::{Rgb, g502::G502, k70::K70};

fn main() {
    env_logger::init();
    match G502::open() {
        Ok(mut g) => {
            println!("G502 serial={} zones={:?}", g.serial(), g.zones.iter().map(|z| (z.index, z.name())).collect::<Vec<_>>());
            g.set_software_control(true).expect("sw control");
            for c in [Rgb(255, 0, 0), Rgb(0, 255, 0), Rgb(0, 0, 255)] {
                g.set_colors(&vec![c; g.zones.len()]).expect("set colors");
                std::thread::sleep(Duration::from_secs(1));
            }
            println!("G502 ok, restoring onboard profile");
        }
        Err(e) => eprintln!("G502: {e}"),
    }

    let mut attempt = 0;
    loop {
        attempt += 1;
        let r = (|| -> lumen_hid::Result<()> {
            let mut k = K70::open()?;
            println!("{} serial={}", k.model.name(), k.serial());
            k.set_software_mode(true)?;
            for step in 0..40 {
                let level = ((step as f32 / 39.0 * std::f32::consts::PI).sin() * 255.0) as u8;
                k.write_colors(&vec![Rgb(level, level / 3, 0); k.model.leds()])?;
                std::thread::sleep(Duration::from_millis(75));
            }
            Ok(())
        })();
        match r {
            Ok(()) => { println!("K70 ok, restoring onboard lighting"); break; }
            Err(e) if attempt < 3 => { eprintln!("K70 attempt {attempt}: {e}; retrying (keyboard may have reset)"); std::thread::sleep(Duration::from_secs(4)); }
            Err(e) => { eprintln!("K70: {e}"); break; }
        }
    }
}
