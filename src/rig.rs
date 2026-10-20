//! The rig file (`rig.toml`): everything specific to one computer + scanner
//! setup. The same experiment runs on your laptop and at the scanner by
//! swapping the rig file.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rig {
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub trigger: Trigger,
    /// Raw key or serial character -> button label used in experiments.
    /// e.g. `"1" = "index"`. Defaults to keys 1-4 mapping to themselves.
    #[serde(default = "default_buttons")]
    pub buttons: BTreeMap<String, String>,
    #[serde(default)]
    pub photodiode: Photodiode,
    #[serde(default)]
    pub output: Output,
    #[serde(default)]
    pub site: Site,
}

fn default_buttons() -> BTreeMap<String, String> {
    ["1", "2", "3", "4"].iter().map(|k| (k.to_string(), k.to_string())).collect()
}

impl Default for Rig {
    fn default() -> Self {
        toml::from_str("").expect("empty rig parses")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    /// Monitor index (see `onset displays`) or part of its name. Defaults to
    /// the last connected monitor, which is usually the projector.
    pub monitor: Option<MonitorSelect>,
    #[serde(default = "yes")]
    pub fullscreen: bool,
    /// Window size when not fullscreen (logical pixels).
    #[serde(default = "default_window_size")]
    pub window_size: [u32; 2],
    /// Warn if the measured refresh rate differs from this by more than 1%.
    pub expected_hz: Option<f64>,
    #[serde(default = "yes")]
    pub hide_cursor: bool,
}

impl Default for Display {
    fn default() -> Self {
        Display {
            monitor: None,
            fullscreen: true,
            window_size: default_window_size(),
            expected_hz: None,
            hide_cursor: true,
        }
    }
}

fn yes() -> bool {
    true
}
fn default_window_size() -> [u32; 2] {
    [1280, 800]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MonitorSelect {
    Index(usize),
    Name(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    #[serde(default)]
    pub source: TriggerSource,
    /// Keyboard mode: the key the trigger box types (fORP default "5").
    #[serde(default = "default_trigger_key")]
    pub key: String,
    /// USB mode ("hid"): the device's USB id "vendor:product" in hex, e.g.
    /// "181b:0008" for a Current Designs fORP 932 (`onset ports` lists them).
    pub device: Option<String>,
    /// Serial mode: port path, e.g. "/dev/cu.usbserial-1410" (see `onset ports`).
    pub port: Option<String>,
    #[serde(default = "default_baud")]
    pub baud: u32,
    /// Serial mode: the character that means "trigger" (others are buttons).
    #[serde(default = "default_trigger_key")]
    pub serial_char: String,
    /// Simulated mode: seconds between fake triggers.
    #[serde(default = "default_sim_tr")]
    pub simulated_tr: f64,
}

impl Default for Trigger {
    fn default() -> Self {
        Trigger {
            source: TriggerSource::Keyboard,
            key: default_trigger_key(),
            device: None,
            port: None,
            baud: default_baud(),
            serial_char: default_trigger_key(),
            simulated_tr: default_sim_tr(),
        }
    }
}

fn default_trigger_key() -> String {
    "5".into()
}
fn default_baud() -> u32 {
    115_200
}
fn default_sim_tr() -> f64 {
    2.0
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    #[default]
    Keyboard,
    Serial,
    /// A USB keyboard-style trigger box read directly, whatever has focus.
    Hid,
    Simulated,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Photodiode {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub corner: Corner,
    /// Side length in logical pixels.
    #[serde(default = "default_patch_size")]
    pub size: f32,
    /// How many frames the patch stays white at each stimulus onset.
    #[serde(default = "default_flash_frames")]
    pub flash_frames: u32,
}

impl Default for Photodiode {
    fn default() -> Self {
        Photodiode {
            enabled: true,
            corner: Corner::default(),
            size: default_patch_size(),
            flash_frames: default_flash_frames(),
        }
    }
}

fn default_patch_size() -> f32 {
    40.0
}
fn default_flash_frames() -> u32 {
    3
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    #[default]
    BottomLeft,
    BottomRight,
    TopLeft,
    TopRight,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(default = "default_out_dir")]
    pub dir: PathBuf,
}

impl Default for Output {
    fn default() -> Self {
        Output { dir: default_out_dir() }
    }
}

fn default_out_dir() -> PathBuf {
    "data".into()
}

/// Where the scans happen, for the welcome screen.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// Logo (PNG or JPEG) in a top corner of the welcome screen. Relative to
    /// the rig file; `~/` is the home folder.
    pub logo: Option<PathBuf>,
}

/// `p` from a rig file at `rig_file`: `~/` is the home folder, relative paths
/// are relative to the rig file.
fn resolve(rig_file: &Path, p: &Path) -> PathBuf {
    if let (Ok(rest), Some(home)) = (p.strip_prefix("~"), std::env::var_os("HOME")) {
        return PathBuf::from(home).join(rest);
    }
    rig_file.parent().unwrap_or(Path::new("")).join(p)
}

impl Rig {
    pub fn load(path: &Path) -> Result<Rig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("can't read rig file {}", path.display()))?;
        let mut rig: Rig = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}:\n{e}", path.display()))?;
        rig.site.logo = rig.site.logo.map(|logo| resolve(path, &logo));
        if rig.trigger.source == TriggerSource::Hid
            && rig.trigger.device.as_deref().and_then(crate::input::parse_usb_id).is_none()
        {
            bail!(
                "{}: [trigger] source = \"hid\" needs `device = \"vendor:product\"` in hex, e.g. \"181b:0008\" \
                 (`onset ports` lists USB devices)",
                path.display()
            );
        }
        if rig.trigger.source == TriggerSource::Serial && rig.trigger.port.is_none() {
            bail!("{}: [trigger] source = \"serial\" needs a `port`", path.display());
        }
        if rig.trigger.serial_char.chars().count() != 1 {
            bail!("{}: [trigger] serial_char must be a single character", path.display());
        }
        Ok(rig)
    }

    /// Map a raw key/character to a button label, if it is one.
    pub fn button_for(&self, raw: &str) -> Option<&str> {
        self.buttons.get(raw).map(String::as_str)
    }

    pub fn is_known_button(&self, label: &str) -> bool {
        self.buttons.values().any(|b| b == label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_paths_are_relative_to_the_rig_file() {
        let rig = Path::new("examples/rigs/scanner.toml");
        assert_eq!(resolve(rig, Path::new("logo.png")), Path::new("examples/rigs/logo.png"));
        assert_eq!(resolve(rig, Path::new("/srv/logo.png")), Path::new("/srv/logo.png"));
        let home = std::env::var("HOME").unwrap();
        assert_eq!(resolve(rig, Path::new("~/assets/logo.png")), Path::new(&home).join("assets/logo.png"));
    }
}
