//! Sends a test pattern (or the image given as argv[1]) to the AIO LCD for 15 s, then releases it.

use std::time::{Duration, Instant};

use image::{ImageBuffer, Rgb, imageops::FilterType};
use lumen_hid::icue_lcd::{HEIGHT, Lcd, WIDTH};

fn main() {
    env_logger::init();
    let lcd = Lcd::open().expect("open LCD");
    println!("LCD serial={}", lcd.serial());

    let img = match std::env::args().nth(1) {
        Some(path) => image::open(path)
            .expect("load image")
            .resize_to_fill(WIDTH, HEIGHT, FilterType::Lanczos3)
            .to_rgb8(),
        None => ImageBuffer::from_fn(WIDTH, HEIGHT, |x, y| {
            let (cx, cy) = (x as f32 - 240.0, y as f32 - 240.0);
            let r = (cx * cx + cy * cy).sqrt();
            if (150.0..170.0).contains(&r) {
                Rgb([0x81, 0xa1, 0xc1]) // Nord accent ring
            } else {
                Rgb([(x * 255 / WIDTH) as u8, 40, (y * 255 / HEIGHT) as u8])
            }
        }),
    };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
        .encode_image(&img)
        .expect("encode");
    println!("frame: {} bytes", jpeg.len());

    lcd.set_brightness(100).expect("brightness");
    let start = Instant::now();
    let mut frames = 0;
    while start.elapsed() < Duration::from_secs(std::env::var("SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15)) {
        lcd.send_jpeg(&jpeg).expect("send frame");
        frames += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("sent {frames} frames, releasing panel");
    lcd.release().expect("release");
}
