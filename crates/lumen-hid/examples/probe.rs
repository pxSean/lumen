//! Read-only hardware probe: lists HID nodes, then enumerates the iCUE LINK hub and reads sensors.
//! The hub is briefly put in software mode to enumerate and returned to hardware mode on exit.

use lumen_hid::{hidraw, icue_link::LinkHub};

fn main() {
    env_logger::init();
    for i in hidraw::enumerate() {
        println!(
            "{} {:04x}:{:04x} if={:?} {} serial={:?}",
            i.node.display(), i.vid, i.pid, i.interface, i.name, i.serial
        );
    }
    println!();

    let mut hub = match LinkHub::open() {
        Ok(h) => h,
        Err(e) => return eprintln!("hub: {e}"),
    };
    println!("iCUE LINK hub serial={} firmware={}", hub.serial(), hub.firmware);
    if let Err(e) = hub.enter_software_mode() {
        return eprintln!("software mode/enumerate: {e}");
    }
    for d in &hub.devices {
        println!(
            "  ch{:<2} {:<28} type={:#04x} model={} kind={:?} leds={} serial={}",
            d.channel, d.name, d.type_id, d.model, d.kind, d.leds, d.serial
        );
    }
    println!("  total LEDs: {}", hub.led_count());
    match hub.read_speeds() {
        Ok(s) => println!("  speeds: {s:?}"),
        Err(e) => eprintln!("  speeds: {e}"),
    }
    match hub.read_temps() {
        Ok(t) => println!("  temps:  {t:?}"),
        Err(e) => eprintln!("  temps: {e}"),
    }
}
