//! `onset probe`: record the timing probe's serial output to a TSV file.
//!
//! The Arduino sketch (`hardware/onset_timing_probe`) prints one line per event:
//!
//! ```text
//! P,<us>,1      photodiode light ON  (stimulus onset)
//! P,<us>,0      photodiode light OFF
//! T,<us>        scanner trigger
//! # ...         comments (banner, warnings), echoed to the terminal
//! ```
//!
//! The recording has the columns `kind  arduino_us  level  host_time`.
//! `level` is n/a for triggers. `host_time` is this computer's Unix time when
//! the line arrived: good to a few ms, for bookkeeping, never for timing.
//! Comment lines are kept in the file too. `onset check-timing` reads it.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Arduino's USB vendor IDs (official boards and the older arduino.org ones).
const ARDUINO_VIDS: [u16; 2] = [0x2341, 0x2A03];

/// Column names of a probe recording.
pub const HEADER: &str = "kind\tarduino_us\tlevel\thost_time";

#[derive(clap::Args)]
pub struct Args {
    /// Serial port (default: find the Arduino).
    #[arg(long)]
    port: Option<String>,
    /// Baud rate (the sketch's is 115200).
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
    /// Output TSV (default: probe_<date>_<time>.tsv).
    #[arg(long)]
    out: Option<PathBuf>,
    /// List serial ports and exit.
    #[arg(long)]
    list: bool,
    /// Don't print every event.
    #[arg(long)]
    quiet: bool,
}

/// One event line from the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Photodiode edge: light on (`true`) or off.
    Light {
        us: u64,
        on: bool,
    },
    Trigger {
        us: u64,
    },
}

/// Parse `P,123,1` / `T,456`. `None` for anything else (comments, half lines).
pub fn parse_line(line: &str) -> Option<Event> {
    let parts: Vec<&str> = line.trim().split(',').collect();
    match parts.as_slice() {
        ["P", us, level @ ("0" | "1")] => Some(Event::Light { us: us.parse().ok()?, on: *level == "1" }),
        ["T", us] => Some(Event::Trigger { us: us.parse().ok()? }),
        _ => None,
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub triggers: usize,
    pub edges: usize,
    /// Lines that were neither events nor comments (e.g. a half line right
    /// after the port opened).
    pub bad: usize,
}

/// Copy probe lines from `input` to `out` as TSV rows (without the header)
/// until `stop` is set or the input ends. Read timeouts just mean nothing
/// arrived yet. `host_time` gives each line's arrival time.
pub fn record(
    input: &mut impl BufRead,
    out: &mut impl Write,
    stop: &AtomicBool,
    quiet: bool,
    counts: &mut Counts,
    host_time: impl Fn() -> f64,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let at_end = match input.read_until(b'\n', &mut buf) {
            Ok(0) => true,
            // A partial line stays in `buf` until the rest arrives.
            Ok(_) if !buf.ends_with(b"\n") => continue,
            Ok(_) => false,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        let line = String::from_utf8_lossy(&buf).trim().to_string();
        buf.clear();
        if !line.is_empty() {
            handle_line(&line, out, quiet, counts, host_time())?;
        }
        if at_end {
            break;
        }
    }
    // Stopped halfway through a line.
    if !String::from_utf8_lossy(&buf).trim().is_empty() {
        counts.bad += 1;
    }
    Ok(())
}

fn handle_line(
    line: &str,
    out: &mut impl Write,
    quiet: bool,
    counts: &mut Counts,
    host_time: f64,
) -> std::io::Result<()> {
    if line.starts_with('#') {
        eprintln!("{line}");
        writeln!(out, "{line}")?;
        return out.flush();
    }
    let Some(event) = parse_line(line) else {
        counts.bad += 1;
        return Ok(());
    };
    let (kind, us, level, label) = match event {
        Event::Light { us, on } => {
            counts.edges += 1;
            ("P", us, if on { "1" } else { "0" }, if on { "light on" } else { "light off" })
        }
        Event::Trigger { us } => {
            counts.triggers += 1;
            ("T", us, "n/a", "trigger")
        }
    };
    writeln!(out, "{kind}\t{us}\t{level}\t{host_time:.3}")?;
    // Flush every line so a crash or a yanked cable loses nothing.
    out.flush()?;
    if !quiet {
        eprintln!(
            "{:12.6} s  {label:9}  (triggers {}, edges {})",
            us as f64 / 1e6,
            counts.triggers,
            counts.edges
        );
    }
    Ok(())
}

fn describe(p: &serialport::SerialPortInfo) -> String {
    match &p.port_type {
        serialport::SerialPortType::UsbPort(u) => format!(
            "USB {:04x}:{:04x} {} {}",
            u.vid,
            u.pid,
            u.manufacturer.as_deref().unwrap_or_default(),
            u.product.as_deref().unwrap_or_default()
        ),
        other => format!("{other:?}"),
    }
}

/// Pick the Arduino's serial port, or explain how to choose one.
fn find_port() -> Result<String> {
    let ports = serialport::available_ports().context("can't list serial ports")?;
    // macOS lists each device twice; read from the /dev/cu.* one (the
    // /dev/tty.* one waits for a modem carrier signal).
    let cu: Vec<_> = ports.iter().filter(|p| p.port_name.starts_with("/dev/cu.")).collect();
    let is_arduino = |p: &&&serialport::SerialPortInfo| matches!(&p.port_type, serialport::SerialPortType::UsbPort(u) if ARDUINO_VIDS.contains(&u.vid));
    let mut found: Vec<_> = cu.iter().filter(is_arduino).collect();
    if found.is_empty() {
        // Clones use other USB chips (CH340, FTDI); fall back to the names.
        found = cu
            .iter()
            .filter(|p| p.port_name.contains("usbmodem") || p.port_name.contains("usbserial"))
            .collect();
    }
    if let [one] = found.as_slice() {
        return Ok(one.port_name.clone());
    }
    let mut listing: String = ports.iter().map(|p| format!("\n  {}  {}", p.port_name, describe(p))).collect();
    if listing.is_empty() {
        listing = "\n  (none)".into();
    }
    if found.is_empty() {
        bail!("no Arduino found. Is it plugged in? Serial ports:{listing}");
    }
    bail!("several candidate ports; pick one with --port:{listing}");
}

fn unix_time() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

pub fn run(args: Args) -> Result<()> {
    if args.list {
        for p in serialport::available_ports().context("can't list serial ports")? {
            println!("{}\t{}", p.port_name, describe(&p));
        }
        return Ok(());
    }
    let port_name = match args.port {
        Some(p) => p,
        None => find_port()?,
    };
    let out_path = args.out.unwrap_or_else(|| {
        PathBuf::from(format!("probe_{}.tsv", chrono::Local::now().format("%Y%m%d_%H%M%S")))
    });
    if let Some(dir) = out_path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
    }

    // Opening the port resets an Uno R3 (it reboots and prints its banner
    // again after ~2 s). Leonardo and Uno R4 don't reset.
    let port = serialport::new(&port_name, args.baud)
        .timeout(Duration::from_millis(500))
        .open()
        .with_context(|| format!("can't open {port_name}"))?;
    let mut input = BufReader::new(port);
    let file =
        std::fs::File::create(&out_path).with_context(|| format!("can't create {}", out_path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    writeln!(
        out,
        "# onset probe started {} port={port_name}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
    )?;
    writeln!(out, "{HEADER}")?;
    out.flush()?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .context("can't install the Ctrl-C handler")?;
    eprintln!("reading {port_name} @ {} -> {}   (Ctrl-C to stop)", args.baud, out_path.display());

    let mut counts = Counts::default();
    let result = record(&mut input, &mut out, &stop, args.quiet, &mut counts, unix_time);
    out.flush()?;
    eprintln!(
        "\nsaved {}: {} triggers, {} photodiode edges{}",
        out_path.display(),
        counts.triggers,
        counts.edges,
        if counts.bad > 0 { format!(", {} unparseable lines skipped", counts.bad) } else { String::new() }
    );
    result.with_context(|| format!("lost the connection to {port_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_and_rejects_the_rest() {
        assert_eq!(parse_line("P,123,1"), Some(Event::Light { us: 123, on: true }));
        assert_eq!(parse_line("P,124,0\r"), Some(Event::Light { us: 124, on: false }));
        assert_eq!(parse_line("T,456"), Some(Event::Trigger { us: 456 }));
        for bad in ["P,1,2", "P,1", "T,x", "T,1,1", "# banner", "", ",12"] {
            assert_eq!(parse_line(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn records_events_comments_and_partial_lines() {
        // A half line from before the port opened, a banner, then events.
        let serial = b"5,1\n# onset_timing_probe v1\nT,1000000\nP,1012000,1\nP,1062000,0\nP,107";
        let mut out = Vec::new();
        let mut counts = Counts::default();
        let stop = AtomicBool::new(false);
        record(&mut &serial[..], &mut out, &stop, true, &mut counts, || 1.5).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "# onset_timing_probe v1\nT\t1000000\tn/a\t1.500\nP\t1012000\t1\t1.500\nP\t1062000\t0\t1.500\n"
        );
        assert_eq!(counts, Counts { triggers: 1, edges: 2, bad: 2 });
    }

    /// Returns its bytes once, then times out like an idle serial port,
    /// pressing Ctrl-C (setting `stop`) on the way.
    struct Stalls<'a> {
        data: Option<&'a [u8]>,
        stop: &'a AtomicBool,
    }

    impl std::io::Read for Stalls<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            match self.data.take() {
                Some(d) => {
                    out[..d.len()].copy_from_slice(d);
                    Ok(d.len())
                }
                None => {
                    self.stop.store(true, Ordering::SeqCst);
                    Err(ErrorKind::TimedOut.into())
                }
            }
        }
    }

    #[test]
    fn timeouts_keep_partial_lines_and_stopping_counts_them() {
        let stop = AtomicBool::new(false);
        let mut port = BufReader::new(Stalls { data: Some(b"T,5\nT,"), stop: &stop });
        let (mut out, mut counts) = (Vec::new(), Counts::default());
        record(&mut port, &mut out, &stop, true, &mut counts, || 0.0).unwrap();
        assert_eq!(counts, Counts { triggers: 1, edges: 0, bad: 1 });
    }
}
