pub mod hidraw;
pub mod icue_link;
pub mod g502;
pub mod icue_lcd;
pub mod k70_lux;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot open {0}: {1} (is the udev rule installed?)")]
    Open(PathBuf, #[source] std::io::Error),
    #[error("device not found")]
    NotFound,
    #[error("device disconnected")]
    Disconnected,
    #[error("timed out waiting for device response")]
    Timeout,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);

    pub fn scale(self, f: f32) -> Rgb {
        let f = f.clamp(0.0, 1.0);
        Rgb(
            (self.0 as f32 * f).round() as u8,
            (self.1 as f32 * f).round() as u8,
            (self.2 as f32 * f).round() as u8,
        )
    }
}
