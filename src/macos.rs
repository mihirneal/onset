//! macOS window placement fixes.
//!
//! A program started from a terminal isn't always the active app, and its
//! window can open behind other windows or on another desktop (Space). macOS
//! then reports it as covered and won't draw to it, which would stall a run.

use objc2::msg_send;
use objc2::runtime::AnyObject;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Show `window` on the current Space, in front of everything, now.
pub fn bring_to_front(window: &Window) {
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view = h.ns_view.as_ptr() as *mut AnyObject;
    // NSWindowCollectionBehavior: MoveToActiveSpace | FullScreenPrimary
    const MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;
    const FULL_SCREEN_PRIMARY: usize = 1 << 7;
    unsafe {
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        let _: () = msg_send![ns_window, setCollectionBehavior: MOVE_TO_ACTIVE_SPACE | FULL_SCREEN_PRIMARY];
        let _: () = msg_send![ns_window, orderFrontRegardless];
        let _: () = msg_send![ns_window, makeKeyWindow];
    }
}

/// One-line description of the window's state, for stall diagnostics.
pub fn describe(window: &Window) -> String {
    let Ok(handle) = window.window_handle() else { return "no handle".into() };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return "not AppKit".into() };
    let view = h.ns_view.as_ptr() as *mut AnyObject;
    unsafe {
        let w: *mut AnyObject = msg_send![view, window];
        if w.is_null() {
            return "view has no window".into();
        }
        let visible: bool = msg_send![w, isVisible];
        let on_space: bool = msg_send![w, isOnActiveSpace];
        let occlusion: usize = msg_send![w, occlusionState];
        let key: bool = msg_send![w, isKeyWindow];
        let mini: bool = msg_send![w, isMiniaturized];
        format!(
            "visible={visible} on_active_space={on_space} occlusion_visible={} key={key} minimized={mini}",
            occlusion & 2 != 0
        )
    }
}
