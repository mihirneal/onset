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
                            // A status line wider than the terminal wraps, and
                            // `\r` then only rewinds its last row, leaving a
                            // copy behind on every update. Cut it to fit.
                            line = fit(&l, terminal_width());
                            if live { write!(err, "\r\x1b[2K{NO_WRAP}{line}{WRAP}") } else { Ok(()) }
                        }
                        Msg::Message(m) => {
                            if live {
                                write!(err, "\r\x1b[2K{m}\n{NO_WRAP}{line}{WRAP}")
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
                format!("{} instructions on screen · waiting for the scanner (the first pulse starts the run)", cyan("●"))
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
            Phase::Holding => format!(
                "{} stimulus finished · holding the final screen until the scanner stops · TR {} · Esc stops waiting",
                green("■"),
                s.triggers
            ),
            Phase::Done(_) => String::new(),
        };
        let _ = self.tx.send(Msg::Line(line));
    }
}

/// Turn the terminal's line wrapping off and on around the status line, so
/// even a line that's still too wide (e.g. the window shrank since) is cut
/// off instead of wrapping.
const NO_WRAP: &str = "\x1b[?7l";
const WRAP: &str = "\x1b[?7h";

/// Columns of the terminal stderr is attached to, or 80 if unknown.
fn terminal_width() -> usize {
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        x_pixels: u16,
        y_pixels: u16,
    }
    unsafe extern "C" {
        fn ioctl(fd: i32, request: u64, ...) -> i32;
    }
    // TIOCGWINSZ on macOS.
    const TIOCGWINSZ: u64 = 0x4008_7468;
    let mut ws = WinSize { rows: 0, cols: 0, x_pixels: 0, y_pixels: 0 };
    // SAFETY: TIOCGWINSZ writes one WinSize into the pointer we pass; fd 2
    // is stderr. On failure nothing is written and we fall back to 80.
    let ok = unsafe { ioctl(2, TIOCGWINSZ, &mut ws as *mut WinSize) } == 0;
    if ok && ws.cols > 0 { ws.cols as usize } else { 80 }
}

/// Cut `line` to at most `width - 1` visible characters, keeping its color
/// codes intact (they take no space on screen).
fn fit(line: &str, width: usize) -> String {
    let max = width.saturating_sub(1);
    let mut out = String::with_capacity(line.len());
    let mut visible = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy an escape sequence like "\x1b[33m" whole.
            out.push(c);
            for d in chars.by_ref() {
                out.push(d);
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible == max {
            // Reset colors in case we cut inside a colored run.
            out.push_str("\x1b[0m");
            break;
        }
        out.push(c);
        visible += 1;
    }
    out
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn short_lines_are_untouched() {
        assert_eq!(fit("hello", 80), "hello");
    }

    #[test]
    fn long_lines_are_cut_to_the_width() {
        assert_eq!(fit("abcdefghij", 6), "abcde\x1b[0m");
    }

    #[test]
    fn color_codes_take_no_space() {
        let colored = "\x1b[36m●\x1b[0m waiting";
        assert_eq!(fit(colored, 80), colored);
        assert_eq!(fit(colored, 4), "\x1b[36m●\x1b[0m w\x1b[0m");
    }
}
