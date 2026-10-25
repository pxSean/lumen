//! Turns the AIO LCD panel off and back on using OpenLinkHub's scheduler sequence
//! (brightness 0 + panel power command, then brightness restore). Recovery tool.

use std::time::Duration;

use lumen_hid::icue_lcd::Lcd;

fn main() {
    let lcd = Lcd::open().expect("open LCD");
    println!("panel off");
    lcd.set_brightness(0).expect("brightness 0");
    lcd.send_feature_raw(&[0x03, 0x19, 0x00, 0x01]).expect("panel power");
    std::thread::sleep(Duration::from_secs(3));
    println!("panel on");
    lcd.set_brightness(100).expect("brightness 100");
    std::thread::sleep(Duration::from_secs(1));
    lcd.release().expect("release");
    println!("done");
}
