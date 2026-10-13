//! Trigger and response sources that run on their own threads.
//!
//! Each source timestamps an event the moment it arrives (before anything
//! else happens) and sends it down a channel. The render loop drains the
//! channel every frame, so a button press is timed precisely even though it
//! is *processed* up to one frame later.
//!
//! Keyboard input doesn't come through here: winit delivers it on the main
//! thread, where it is timestamped on arrival (accurate to a few ms).

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
    Simulated,
    /// The simulated participant (`--robot`).
    Robot,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Keyboard => "keyboard",
            Source::Serial => "serial",
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
