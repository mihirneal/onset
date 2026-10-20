//! Trigger and response sources that run on their own threads.
//!
//! Each source timestamps an event the moment it arrives (before anything
//! else happens) and sends it down a channel. The render loop drains the
//! channel every frame, so a button press is timed precisely even though it
//! is *processed* up to one frame later.
//!
//! Keyboard input from the focused window doesn't come through here: winit
//! delivers it on the main thread. A trigger box that acts as a USB keyboard
//! (e.g. a fORP) can instead be read directly with `spawn_hid`, which works
//! whichever window is in front.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Keyboard,
    Serial,
    /// A USB HID keyboard read directly (trigger box / button pads).
    Hid,
    Simulated,
    /// The simulated participant (`--robot`).
    Robot,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Keyboard => "keyboard",
            Source::Serial => "serial",
            Source::Hid => "hid",
            Source::Simulated => "simulated",
            Source::Robot => "robot",
        }
    }
}

#[derive(Debug, Clone)]
pub enum RawInput {
    /// A key or character, timestamped on arrival.
    Key { t: Instant, source: Source, raw: String },
    /// A source failed (e.g. serial cable unplugged).
    Error(String),
}

/// Read single characters from a serial port (e.g. a fORP in serial mode or
/// an Arduino trigger converter). Each byte becomes one input.
pub fn spawn_serial(
    port: &str,
    baud: u32,
    tx: Sender<RawInput>,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let mut serial =
        serialport::new(port, baud).timeout(Duration::from_millis(50)).open().with_context(|| {
            let ports = serialport::available_ports()
                .map(|ps| ps.into_iter().map(|p| p.port_name).collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            format!("can't open serial port {port} (available: {ports})")
        })?;
    // Drop anything buffered before the run started.
    let _ = serial.clear(serialport::ClearBuffer::Input);
    Ok(std::thread::Builder::new().name("onset-serial".into()).spawn(move || {
        let mut buf = [0u8; 64];
        while !stop.load(Ordering::Relaxed) {
            match serial.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    let t = Instant::now();
                    for &b in &buf[..n] {
                        if b == b'\r' || b == b'\n' {
                            continue;
                        }
                        let raw = (b as char).to_string();
                        if tx.send(RawInput::Key { t, source: Source::Serial, raw }).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    let _ = tx.send(RawInput::Error(format!("serial port error: {e}")));
                    return;
                }
            }
        }
    })?)
}

/// Read a USB keyboard-style device (a trigger box or button pads, e.g. a
/// fORP in keyboard mode) directly, on its own thread. Each key press becomes
/// one input, timestamped on arrival, no matter which window has focus.
///
/// The device is opened exclusively when macOS allows it, so its keys stop
/// reaching other apps (no stray "t"s typed into whatever is in front). On
/// macOS this needs the Input Monitoring permission for the app Onset runs
/// in; `macos::request_input_monitoring` asks for it.
pub fn spawn_hid(vendor: u16, product: u16, tx: Sender<RawInput>, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    // Everything happens on the reader thread, including starting the HID
    // library: on macOS that briefly runs the calling thread's event loop,
    // which would re-enter the window system if done on the main thread.
    // The thread reports back whether it could open the device.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let handle = std::thread::Builder::new().name("onset-hid".into()).spawn(move || {
        let device = match open_hid(vendor, product) {
            Ok(d) => {
                let _ = ready_tx.send(Ok(()));
                d
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let mut buf = [0u8; 64];
        let mut held: Vec<u8> = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match device.read_timeout(&mut buf, 50) {
                Ok(0) => {}
                Ok(n) => {
                    let t = Instant::now();
                    let now_held = keys_in_report(&buf[..n]);
                    // A key counts once, when it first appears in a report.
                    for &usage in now_held.iter().filter(|u| !held.contains(u)) {
                        if let Some(c) = usage_char(usage) {
                            let ev = RawInput::Key { t, source: Source::Hid, raw: c.to_string() };
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                    held = now_held;
                }
                Err(e) => {
                    let _ = tx.send(RawInput::Error(format!("USB trigger device error: {e}")));
                    return;
                }
            }
        }
    })?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("the USB reader thread stopped before opening the device"),
    }
}

/// Find the device's keyboard part and open it, exclusively if macOS allows.
fn open_hid(vendor: u16, product: u16) -> Result<hidapi::HidDevice> {
    let api = hidapi::HidApi::new().context("can't start the USB HID library")?;
    let matching: Vec<&hidapi::DeviceInfo> =
        api.device_list().filter(|d| d.vendor_id() == vendor && d.product_id() == product).collect();
    // A device can show up several times (keyboard, mouse, joystick parts);
    // we want its keyboard part: usage page 1 (generic desktop), usage 6.
    let info = matching
        .iter()
        .find(|d| d.usage_page() == 1 && d.usage() == 6)
        .or_else(|| matching.first())
        .with_context(|| format!("no USB device {vendor:04x}:{product:04x} connected (`onset ports` lists them)"))?;
    let open = |exclusive: bool| {
        api.set_open_exclusive(exclusive);
        api.open_path(info.path())
    };
    open(true).or_else(|_| open(false)).with_context(|| {
        format!(
            "can't read USB device {vendor:04x}:{product:04x}. On macOS, allow Input Monitoring for the app \
             running Onset (System Settings → Privacy & Security → Input Monitoring), then restart it"
        )
    })
}

/// The key usage codes held down in a keyboard input report:
/// `[modifiers, reserved, key1..key6]`, preceded by a report ID byte when the
/// report is 9 bytes long.
fn keys_in_report(report: &[u8]) -> Vec<u8> {
    let offset = if report.len() == 9 { 1 } else { 0 };
    report.iter().skip(offset + 2).take(6).copied().filter(|&u| u > 0x03).collect()
}

/// The character a USB keyboard usage code types, for letters and digits.
fn usage_char(usage: u8) -> Option<char> {
    match usage {
        0x04..=0x1d => Some((b'a' + (usage - 0x04)) as char),
        0x1e..=0x26 => Some((b'1' + (usage - 0x1e)) as char),
        0x27 => Some('0'),
        _ => None,
    }
}

/// Parse "181b:0008" or "0x181b:0x0008" into (vendor, product).
pub fn parse_usb_id(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    let hex = |x: &str| u16::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
    Some((hex(v)?, hex(p)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_from_usage_codes() {
        assert_eq!(usage_char(0x17), Some('t'));
        assert_eq!(usage_char(0x05), Some('b'));
        assert_eq!(usage_char(0x1c), Some('y'));
        assert_eq!(usage_char(0x22), Some('5'));
        assert_eq!(usage_char(0x27), Some('0'));
        assert_eq!(usage_char(0x28), None); // Enter
    }

    #[test]
    fn keys_in_plain_and_numbered_reports() {
        assert_eq!(keys_in_report(&[0, 0, 0x17, 0, 0, 0, 0, 0]), vec![0x17]);
        assert_eq!(keys_in_report(&[1, 0, 0, 0x17, 0x05, 0, 0, 0, 0]), vec![0x17, 0x05]);
        assert!(keys_in_report(&[0; 8]).is_empty());
    }

    #[test]
    fn usb_ids() {
        assert_eq!(parse_usb_id("181b:0008"), Some((0x181b, 0x0008)));
        assert_eq!(parse_usb_id("0x181B:0x0008"), Some((0x181b, 0x0008)));
        assert_eq!(parse_usb_id("181b"), None);
    }
}

/// A fake scanner: sends `raw` every `tr` seconds, starting `first_after`
/// seconds from now, on a fixed grid (no drift).
pub fn spawn_simulated_scanner(
    tr: f64,
    first_after: f64,
    raw: String,
    tx: Sender<RawInput>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("onset-fake-scanner".into())
        .spawn(move || {
            let start = Instant::now() + Duration::from_secs_f64(first_after);
            for n in 0u64.. {
                let due = start + Duration::from_secs_f64(tr * n as f64);
                // Sleep most of the way, then spin for precision.
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    if now >= due {
                        break;
                    }
                    let left = due - now;
                    if left > Duration::from_millis(3) {
                        std::thread::sleep((left - Duration::from_millis(2)).min(Duration::from_millis(50)));
                    } else {
                        std::hint::spin_loop();
                    }
                }
                let ev = RawInput::Key { t: Instant::now(), source: Source::Simulated, raw: raw.clone() };
                if tx.send(ev).is_err() {
                    return;
                }
            }
        })
        .expect("spawn simulated scanner thread")
}
