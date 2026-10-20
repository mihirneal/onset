//! Log scanner trigger pulses and report the measured TR.
//!
//! With a trigger box in keyboard mode (the fORP 932 in BYGRT mode), every
//! pulse arrives as a keypress, so click into this terminal before the scan
//! starts. Other keys (button presses) are logged too.
//!
//!     cargo run --bin trigger-log -- --out logs/dummy-run.tsv
//!     cargo run --bin trigger-log -- --simulate 2.0 --volumes 10
//!
//! Times are taken when the terminal hands us the key, so they include a few
//! ms of USB and terminal delay. That's fine for measuring a TR. True
//! stimulus latency is what the photodiode test is for.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use onset::scanner::{fake_scanner, tr_stats};

#[derive(Parser)]
#[command(about = "Log scanner trigger pulses and report the measured TR")]
struct Args {
    /// The key the trigger box sends for each pulse.
    #[arg(long, default_value_t = 't')]
    key: char,

    /// Also save every keypress to this TSV file.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Use a fake scanner with this TR in seconds instead of the keyboard.
    #[arg(long, value_name = "TR_SECONDS")]
    simulate: Option<f64>,

    /// How many pulses the fake scanner sends.
    #[arg(long, default_value_t = 10)]
    volumes: u32,
}

/// Every keypress we've seen, plus enough state to print each one live.
struct Recorder {
    trigger_key: char,
    started: Instant,
    presses: Vec<(Instant, char)>,
    first_pulse: Option<Instant>,
    last_pulse: Option<Instant>,
    pulse_count: usize,
}

impl Recorder {
    fn new(trigger_key: char) -> Self {
        Recorder {
            trigger_key,
            started: Instant::now(),
            presses: Vec::new(),
            first_pulse: None,
            last_pulse: None,
            pulse_count: 0,
        }
    }

    /// Caps Lock shouldn't turn a `t` pulse into an unknown `T` key.
    fn is_pulse(&self, key: char) -> bool {
        key.eq_ignore_ascii_case(&self.trigger_key)
    }

    /// Store one keypress and return a line describing it.
    fn record(&mut self, at: Instant, key: char) -> String {
        self.presses.push((at, key));

        if !self.is_pulse(key) {
            return match self.first_pulse {
                Some(zero) => format!("key '{key}'      t = {:>9.3} s", secs_since(at, zero)),
                None => format!("key '{key}'      (before the first pulse)"),
            };
        }

        self.pulse_count += 1;
        let zero = *self.first_pulse.get_or_insert(at);
        let t = secs_since(at, zero);
        let line = match self.last_pulse {
            Some(prev) => format!(
                "pulse {:>4}   t = {t:>9.3} s   gap {:.4} s",
                self.pulse_count,
                (at - prev).as_secs_f64()
            ),
            None => format!("pulse {:>4}   t = {t:>9.3} s   (time zero)", self.pulse_count),
        };
        self.last_pulse = Some(at);
        line
    }

    /// Times are relative to the first pulse, or to the logger's start if no
    /// pulse ever came.
    fn zero(&self) -> Instant {
        self.first_pulse.unwrap_or(self.started)
    }

    fn summary(&self) -> Vec<String> {
        let zero = self.zero();
        let pulse_times: Vec<f64> = self
            .presses
            .iter()
            .filter(|(_, key)| self.is_pulse(*key))
            .map(|(at, _)| secs_since(*at, zero))
            .collect();

        let mut lines = vec![String::new(), "--- Summary ---".to_string()];
        match tr_stats(&pulse_times) {
            Some(s) => {
                lines.push(format!(
                    "Pulses: {} over {:.3} s",
                    s.pulses,
                    pulse_times[pulse_times.len() - 1]
                ));
                lines.push(format!(
                    "TR: median {:.4} s, mean {:.4} s",
                    s.median_tr_s, s.mean_tr_s
                ));
                lines.push(format!(
                    "Jitter: SD {:.1} ms, gaps ranged {:.4} to {:.4} s",
                    s.sd_s * 1000.0,
                    s.min_tr_s,
                    s.max_tr_s
                ));
                lines.push(format!("Likely missed pulses: {}", s.likely_missed));
            }
            None if pulse_times.len() == 1 => {
                lines.push("Only one pulse; a TR needs at least two.".to_string());
            }
            None => lines.push(format!("No '{}' pulses received.", self.trigger_key)),
        }

        // BTreeMap keeps the keys sorted, like a dict you iterate in key order.
        let mut others: BTreeMap<char, usize> = BTreeMap::new();
        for (_, key) in self.presses.iter().filter(|(_, key)| !self.is_pulse(*key)) {
            *others.entry(*key).or_default() += 1;
        }
        if !others.is_empty() {
            let counts: Vec<String> = others.iter().map(|(k, n)| format!("'{k}' ×{n}")).collect();
            lines.push(format!("Other keys: {}", counts.join(", ")));
        }
        lines
    }

    fn write_tsv(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        let mut out = BufWriter::new(File::create(path)?);
        writeln!(out, "time_s\tkind\tkey")?;
        let zero = self.zero();
        for (at, key) in &self.presses {
            let kind = if self.is_pulse(*key) { "pulse" } else { "key" };
            writeln!(out, "{:.6}\t{kind}\t{key}", secs_since(*at, zero))?;
        }
        out.flush()
    }
}

/// Seconds from `zero` to `at`, negative if `at` came first.
fn secs_since(at: Instant, zero: Instant) -> f64 {
    if at >= zero {
        (at - zero).as_secs_f64()
    } else {
        -(zero - at).as_secs_f64()
    }
}

/// Raw mode hands us each key the moment it's pressed, instead of waiting for
/// Enter. Dropping this guard switches it back off, even if we bail out with
/// an error, so the terminal is never left in a broken state.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn listen_to_keyboard(rec: &mut Recorder) -> io::Result<()> {
    let _raw = RawMode::enable()?;
    let mut stdout = io::stdout();
    // Raw mode also stops "\n" from returning to the start of the line.
    write!(
        stdout,
        "Waiting for '{}' pulses. Keep this window in front. Esc or Ctrl-C stops.\r\n",
        rec.trigger_key
    )?;
    stdout.flush()?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let at = Instant::now();
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Char(c) => {
                write!(stdout, "{}\r\n", rec.record(at, c))?;
                stdout.flush()?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn listen_to_fake_scanner(rec: &mut Recorder, tr_s: f64, volumes: u32) {
    println!("Fake scanner: {volumes} pulses, TR {tr_s} s. First pulse in {tr_s} s.");
    for at in fake_scanner(Duration::from_secs_f64(tr_s), volumes) {
        let key = rec.trigger_key;
        println!("{}", rec.record(at, key));
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let mut rec = Recorder::new(args.key);

    match args.simulate {
        Some(tr_s) if !(tr_s.is_finite() && tr_s > 0.0) => {
            eprintln!("--simulate needs a TR above zero, got {tr_s}");
            process::exit(2);
        }
        Some(tr_s) => listen_to_fake_scanner(&mut rec, tr_s, args.volumes),
        None => listen_to_keyboard(&mut rec)?,
    }

    for line in rec.summary() {
        println!("{line}");
    }
    if let Some(path) = &args.out {
        rec.write_tsv(path)?;
        println!("Saved {}", path.display());
    }
    Ok(())
}
