//! Minimal Linux hidraw access: enumeration via sysfs, plus read/write/feature-report ioctls.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Debug, Clone)]
pub struct HidInfo {
    pub node: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub interface: Option<u8>,
    pub name: String,
    pub serial: String,
}

/// Lists every `/dev/hidraw*` node with its USB identity.
pub fn enumerate() -> Vec<HidInfo> {
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return Vec::new();
    };
    let mut out: Vec<HidInfo> = entries
        .flatten()
        .filter_map(|e| info_for(&e.path()))
        .collect();
    out.sort_by(|a, b| a.node.cmp(&b.node));
    out
}

fn info_for(sys: &Path) -> Option<HidInfo> {
    let uevent = fs::read_to_string(sys.join("device/uevent")).ok()?;
    let mut info = HidInfo {
        node: Path::new("/dev").join(sys.file_name()?),
        vid: 0,
        pid: 0,
        interface: None,
        name: String::new(),
        serial: String::new(),
    };
    for line in uevent.lines() {
        if let Some(id) = line.strip_prefix("HID_ID=") {
            // bus:vendor:product, each zero-padded hex
            let mut parts = id.split(':').skip(1);
            info.vid = u32::from_str_radix(parts.next()?, 16).ok()? as u16;
            info.pid = u32::from_str_radix(parts.next()?, 16).ok()? as u16;
        } else if let Some(name) = line.strip_prefix("HID_NAME=") {
            info.name = name.trim().to_string();
        } else if let Some(uniq) = line.strip_prefix("HID_UNIQ=") {
            info.serial = uniq.to_string();
        }
    }
    // device -> .../<usb-interface>/<hid-device>; the interface number lives one level up.
    let dev = fs::canonicalize(sys.join("device")).ok()?;
    info.interface = dev
        .parent()
        .and_then(|p| fs::read_to_string(p.join("bInterfaceNumber")).ok())
        .and_then(|s| u8::from_str_radix(s.trim(), 16).ok());
    Some(info)
}

/// Finds the first node matching vid/pid and (optionally) USB interface number.
pub fn find(vid: u16, pid: u16, interface: Option<u8>) -> Option<HidInfo> {
    enumerate()
        .into_iter()
        .find(|i| i.vid == vid && i.pid == pid && (interface.is_none() || i.interface == interface))
}

pub struct HidDevice {
    file: File,
    pub info: HidInfo,
}

// _IOC(_IOC_READ|_IOC_WRITE, 'H', nr, len)
const fn hid_ioc_rw(nr: u64, len: usize) -> u64 {
    (3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | nr
}

impl HidDevice {
    pub fn open(info: HidInfo) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&info.node)
            .map_err(|e| Error::Open(info.node.clone(), e))?;
        Ok(Self { file, info })
    }

    /// Writes an output report. `buf[0]` is the report ID (0 for devices without numbered reports).
    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        Ok((&self.file).write(buf)?)
    }

    /// Reads one input report, waiting at most `timeout_ms`. Returns 0 on timeout.
    pub fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if rc == 0 {
            return Ok(0);
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(Error::Disconnected);
        }
        Ok((&self.file).read(buf)?)
    }

    /// Drops any queued input reports.
    pub fn drain(&self) {
        let mut buf = [0u8; 1024];
        while matches!(self.read_timeout(&mut buf, 0), Ok(n) if n > 0) {}
    }

    /// HIDIOCSFEATURE. `buf[0]` is the report ID.
    pub fn send_feature(&self, buf: &[u8]) -> Result<()> {
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_ioc_rw(0x06, buf.len()) as _,
                buf.as_ptr(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// HIDIOCGFEATURE. Set `buf[0]` to the report ID before calling.
    pub fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hid_ioc_rw(0x07, buf.len()) as _,
                buf.as_mut_ptr(),
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(rc as usize)
    }
}
