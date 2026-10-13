//! Onset: stimulus presentation for fMRI.
//!
//! Everything except `render` (GPU drawing) and `input` (hardware threads) is
//! plain logic that's tested without a screen or a scanner. `main.rs` adds
//! the window, frame loop and command line.

pub mod color;
pub mod experiment;
pub mod input;
pub mod output;
pub mod render;
pub mod rig;
pub mod rng;
pub mod run;
pub mod scene;
pub mod schedule;
pub mod template;
pub mod timing;
