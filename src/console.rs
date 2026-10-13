//! The operator's terminal: colors and a live status line.
//!
//! Terminal writes happen on their own thread so a stalled terminal can
//! never delay a frame.

use onset::run::{Phase, Status as RunStatus};
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

fn color_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
fn cyan(s: &str) -> String {
    paint("36", s)
}

enum Msg {
    /// Replace the status line.
    Line(String),
    /// Print a message above the status line.
    Message(String),
    Clear,
}

pub struct Status {
    tx: Sender<Msg>,
}

impl Status {
    pub fn new() -> Status {
        let (tx, rx) = channel::<Msg>();
        let live = std::io::stderr().is_terminal();
        std::thread::Builder::new()
            .name("onset-console".into())
            .spawn(move || {
                let mut err = std::io::stderr();
                let mut line = String::new();
                for msg in rx {
                    let _ = match msg {
                        Msg::Line(l) => {
                            line = l;
                            if live { write!(err, "\r\x1b[2K{line}") } else { Ok(()) }
                        }
                        Msg::Message(m) => {
                            if live {
                                write!(err, "\r\x1b[2K{m}\n{line}")
                            } else {
                                writeln!(err, "{m}")
                            }
                        }
                        Msg::Clear => {
                            line.clear();
                            if live { write!(err, "\r\x1b[2K") } else { Ok(()) }
                        }
                    };
                    let _ = err.flush();
                }
            })
            .expect("spawn console thread");
        Status { tx }
    }

    pub fn message(&mut self, m: &str) {
        let _ = self.tx.send(Msg::Message(m.to_string()));
    }

    pub fn clear(&mut self) {
        let _ = self.tx.send(Msg::Clear);
        // Give the console thread a moment so the summary prints after it.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    pub fn update(&mut self, s: &RunStatus) {
        let line = match s.phase {
            Phase::Instructions => {
                format!("{} instructions on screen · press Space when the participant is ready", cyan("●"))
            }
            Phase::Waiting => format!(
                "{} waiting for scanner trigger{}",
                yellow("●"),
                if s.triggers > 0 { format!(" ({} dummy so far)", s.triggers) } else { String::new() }
            ),
            Phase::Running => {
                let t = s.t.unwrap_or(0.0).max(0.0);
                let frac = (t / s.end.max(1e-9)).clamp(0.0, 1.0);
                let width = 20;
                let filled = (frac * width as f64).round() as usize;
                let bar = format!("{}{}", "█".repeat(filled), dim(&"░".repeat(width - filled)));
                let frames = if s.missed_frames == 0 {
                    green("0 dropped")
                } else {
                    red(&format!("{} dropped", s.missed_frames))
                };
                let resp = match &s.last_response {
                    Some((b, Some(rt))) => format!(" · last {b} {:.0} ms", rt * 1000.0),
                    Some((b, None)) => format!(" · last {b}"),
                    None => String::new(),
                };
                let acc = if s.scored > 0 {
                    format!(" · {}/{} correct", s.correct, s.scored)
                } else {
                    String::new()
                };
                format!(
                    "{} {} {}/{} · TR {} · trial {}/{} {} · {}{}{}",
                    green("▶"),
                    bar,
                    mmss(t),
                    mmss(s.end),
                    s.triggers,
                    s.trial,
                    s.trials,
                    dim(s.event.as_deref().unwrap_or("")),
                    frames,
                    resp,
                    acc
                )
            }
            Phase::Done(_) => String::new(),
        };
        let _ = self.tx.send(Msg::Line(line));
    }
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
