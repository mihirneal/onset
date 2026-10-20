//! The macOS present path end to end (next_frame, draw with wgpu, our own
//! present, Metal's reports), on a Core Animation layer that isn't in any
//! window. Nothing is drawn on screen and no window opens. Metal still
//! reports every frame, with presentedTime 0 because it never reached one.
//!
//! wgpu panics on validation errors by default, so any mistake in wrapping
//! the drawable's texture fails this test.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use objc2_quartz_core::{CALayer, CAMetalLayer};
use onset::render::{Look, Presented, Renderer};

/// Wait up to 2 s for reports on `count` frames.
fn wait_for_reports(renderer: &Renderer, count: usize) -> Vec<Presented> {
    let mut got: Vec<Presented> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while got.len() < count && Instant::now() < deadline {
        got.extend(renderer.presented());
        std::thread::sleep(Duration::from_millis(5));
    }
    got.sort_by_key(|p| p.0);
    got
}

#[test]
fn every_frame_is_drawn_presented_and_reported() {
    let layer = CAMetalLayer::layer();
    let mut renderer = Renderer::on_layer(&layer, 64, 48, 8).expect("a renderer on a bare layer");
    // wgpu's configure turned the 1 s timeout off; ours must be back on.
    assert!(layer.allowsNextDrawableTimeout());
    assert!(!layer.presentsWithTransaction());

    let looks = [
        Look {
            checkerboard: None,
            diode_white: false,
        },
        Look {
            checkerboard: Some(false),
            diode_white: true,
        },
        Look {
            checkerboard: Some(true),
            diode_white: false,
        },
    ];
    for k in 0..6u64 {
        let frame = renderer.next_frame().expect("a frame buffer");
        assert_eq!(frame.id(), k);
        renderer.draw(frame, looks[k as usize % looks.len()]);
    }

    // configure() on resize resets the layer; the timeout must come back.
    renderer.resize(32, 16);
    assert!(layer.allowsNextDrawableTimeout());
    assert_eq!(
        (layer.drawableSize().width, layer.drawableSize().height),
        (32.0, 16.0)
    );
    let frame = renderer
        .next_frame()
        .expect("a frame buffer after resizing");
    assert_eq!(frame.id(), 6);
    renderer.draw(frame, looks[1]);

    let got = wait_for_reports(&renderer, 7);
    let ids: Vec<u64> = got.iter().map(|p| p.0).collect();
    assert_eq!(
        ids,
        (0..7).collect::<Vec<u64>>(),
        "every frame reported exactly once"
    );
    // Off screen, Metal says "never shown" (0.0). Never a garbage time.
    assert!(got.iter().all(|p| p.1.is_finite() && p.1 >= 0.0), "{got:?}");
}

/// Before each frame the presenter walks up the layer tree to find the
/// window and skips the wait if it's hidden. A layer tree with no window at
/// the top (no layer has a delegate) must count as visible, not hang or
/// crash.
#[test]
fn a_layer_tree_without_a_window_still_draws() {
    let parent = CALayer::new();
    let layer = CAMetalLayer::layer();
    parent.addSublayer(&layer);
    let mut renderer = Renderer::on_layer(&layer, 32, 32, 4).expect("a renderer on a bare layer");
    for k in 0..3u64 {
        let frame = renderer.next_frame().expect("a frame buffer");
        assert_eq!(frame.id(), k);
        renderer.draw(
            frame,
            Look {
                checkerboard: None,
                diode_white: false,
            },
        );
    }
    assert_eq!(wait_for_reports(&renderer, 3).len(), 3);
}
