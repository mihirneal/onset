//! Turning named stimuli into drawable items.
//!
//! Coordinates are logical pixels with (0, 0) at the screen center and +y up,
//! like PsychoPy. The renderer converts to physical pixels for HiDPI screens.

use crate::color::Color;
use crate::experiment::{Experiment, StimulusDef};
use crate::template::{Vars, fill};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
    },
    Circle {
        x: f32,
        y: f32,
        r: f32,
        color: Color,
        border: f32,
        border_color: Color,
    },
    Checker {
        x: f32,
        y: f32,
        inner: f32,
        outer: f32,
        rings: u32,
        wedges: u32,
        reversal_hz: f32,
        /// Which contrast phase to draw; the runtime flips this over time.
        phase: bool,
        a: Color,
        b: Color,
    },
    Image {
        path: PathBuf,
        x: f32,
        y: f32,
        w: Option<f32>,
        h: Option<f32>,
    },
    Text {
        text: String,
        x: f32,
        y: f32,
        size: f32,
        color: Color,
        font: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    pub background: Color,
    pub items: Vec<Item>,
}

impl Scene {
    pub fn text(exp: &Experiment, text: &str) -> Scene {
        let m = &exp.experiment;
        Scene {
            background: m.background,
            items: vec![Item::Text {
                text: text.to_string(),
                x: 0.0,
                y: 0.0,
                size: m.font_size,
                color: m.color,
                font: m.font.clone(),
            }],
        }
    }

    /// Set every checkerboard's contrast phase for `t` seconds after onset.
    pub fn animate(&mut self, t: f64) {
        for item in &mut self.items {
            if let Item::Checker { reversal_hz, phase, .. } = item {
                *phase = (t.max(0.0) * *reversal_hz as f64).floor() as u64 % 2 == 1;
            }
        }
    }

    pub fn is_animated(&self) -> bool {
        self.items.iter().any(|i| matches!(i, Item::Checker { .. }))
    }
}

/// Build the scene for stimulus `name` with this trial's variables.
pub fn build(exp: &Experiment, name: &str, vars: &Vars) -> Result<Scene, String> {
    let mut items = Vec::new();
    push_items(exp, name, vars, &mut items, 0)?;
    Ok(Scene { background: exp.experiment.background, items })
}

fn push_items(
    exp: &Experiment,
    name: &str,
    vars: &Vars,
    out: &mut Vec<Item>,
    depth: usize,
) -> Result<(), String> {
    if depth > 16 {
        return Err(format!("stimulus groups nest too deeply at {name:?} (is there a cycle?)"));
    }
    let def = exp.stimulus(name).ok_or_else(|| {
        let mut known: Vec<&str> = exp.stimuli.keys().map(String::as_str).collect();
        known.push("blank");
        format!("no stimulus named {name:?} (defined: {})", known.join(", "))
    })?;
    let fg = exp.experiment.color;
    match def {
        StimulusDef::Blank => {}
        StimulusDef::Fixation { size, thickness, color, x, y } => {
            let color = color.unwrap_or(fg);
            out.push(Item::Rect { x: *x, y: *y, w: *size, h: *thickness, color });
            out.push(Item::Rect { x: *x, y: *y, w: *thickness, h: *size, color });
        }
        StimulusDef::Text { text, size, color, x, y } => out.push(Item::Text {
            text: fill(text, vars)?,
            x: *x,
            y: *y,
            size: size.unwrap_or(exp.experiment.font_size),
            color: color.unwrap_or(fg),
            font: exp.experiment.font.clone(),
        }),
        StimulusDef::Rect { width, height, color, x, y } => {
            out.push(Item::Rect { x: *x, y: *y, w: *width, h: *height, color: color.unwrap_or(fg) })
        }
        StimulusDef::Circle { radius, color, border_width, border_color, x, y } => out.push(Item::Circle {
            x: *x,
            y: *y,
            r: *radius,
            color: color.unwrap_or(fg),
            border: *border_width,
            border_color: *border_color,
        }),
        StimulusDef::Image { path, width, height, x, y } => out.push(Item::Image {
            path: exp.resolve_path(&fill(path, vars)?),
            x: *x,
            y: *y,
            w: *width,
            h: *height,
        }),
        StimulusDef::Checkerboard {
            radius,
            inner_radius,
            rings,
            wedges,
            reversal_hz,
            color_a,
            color_b,
            x,
            y,
        } => out.push(Item::Checker {
            x: *x,
            y: *y,
            inner: *inner_radius,
            outer: *radius,
            rings: *rings,
            wedges: *wedges,
            reversal_hz: *reversal_hz,
            phase: false,
            a: *color_a,
            b: *color_b,
        }),
        StimulusDef::Group { items } => {
            for item in items {
                push_items(exp, item, vars, out, depth + 1)?;
            }
        }
    }
    Ok(())
}
