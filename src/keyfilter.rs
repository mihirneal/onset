//! Stop a USB trigger box's keystrokes from reaching other apps.
//!
//! A keyboard-style trigger box (a fORP in keyboard mode) is read directly by
//! `input::spawn_hid`. macOS only lets an ordinary app share such a device,
//! not take it over, so every pulse *also* arrives as a keystroke in the app
//! that's in front. There it either types a stray "t" or plays the "can't do
//! that" alert, once per TR.
//!
//! This module drops exactly those keystrokes. The direct USB reader sees
//! each press a fraction of a millisecond before macOS turns it into a
//! keystroke and records it with `saw_usb_key`; a system event tap then
//! drops a key-down (and its key-up) that matches a press the reader saw
//! moments earlier. Keys typed on the laptop's own keyboard don't match and
//! pass through untouched.
//!
//! Dropping other apps' keystrokes needs macOS's Accessibility permission for
//! the app Onset runs in (granted once). Without it Onset still works; the
//! stray keystrokes just aren't filtered.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A keystroke counts as the trigger box's if the USB reader saw the same key
/// within this long before it.
const MATCH_WINDOW: Duration = Duration::from_millis(100);

/// If a keystroke arrives before the USB reader has recorded it (rare), wait
/// up to this long for the reader to catch up before letting it through.
const CATCH_UP: Duration = Duration::from_millis(8);

/// Per macOS virtual key code: when the USB reader last saw that key, as
/// nanoseconds since `clock_zero()` (0 = never), and whether a key-up for a
/// dropped key-down is still due.
struct Table {
    last_seen_ns: [AtomicU64; 128],
    drop_up: [AtomicBool; 128],
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| Table {
        last_seen_ns: std::array::from_fn(|_| AtomicU64::new(0)),
        drop_up: std::array::from_fn(|_| AtomicBool::new(false)),
    })
}

fn clock_zero() -> Instant {
    static ZERO: OnceLock<Instant> = OnceLock::new();
    *ZERO.get_or_init(Instant::now)
}

fn now_ns() -> u64 {
    // +1 so that a real time is never 0, which means "never seen".
    clock_zero().elapsed().as_nanos() as u64 + 1
}

/// Called by the USB reader for every key press it sees (`usage` is the USB
/// HID key code). Cheap enough to call always.
pub fn saw_usb_key(usage: u8) {
    if let Some(vk) = mac_key_code(usage) {
        table().last_seen_ns[vk as usize].store(now_ns(), Ordering::Release);
    }
}

/// Should a key-down with this macOS virtual key code be dropped?
fn is_trigger_box_key(vk: u16) -> bool {
    let Some(slot) = table().last_seen_ns.get(vk as usize) else { return false };
    let deadline = Instant::now() + CATCH_UP;
    loop {
        let seen = slot.load(Ordering::Acquire);
        if seen != 0 && now_ns().saturating_sub(seen) <= MATCH_WINDOW.as_nanos() as u64 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_micros(250));
    }
}

/// The macOS virtual key code (ANSI layout) for a USB HID key usage, for
/// letters and digits.
pub fn mac_key_code(usage: u8) -> Option<u16> {
    // USB usages 0x04..=0x1d are a..z; 0x1e..=0x27 are 1..9, 0.
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, // a..m
        0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06, // n..z
    ];
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D]; // 1..9, 0
    match usage {
        0x04..=0x1d => Some(LETTERS[(usage - 0x04) as usize]),
        0x1e..=0x27 => Some(DIGITS[(usage - 0x1e) as usize]),
        _ => None,
    }
}

/// Start filtering on its own thread. Returns `false` if macOS hasn't granted
/// the Accessibility permission (it then shows its own prompt, the first
/// time) or the filter couldn't start.
#[cfg(target_os = "macos")]
pub fn start() -> bool {
    use std::ffi::c_void;
    use std::sync::mpsc;

    type CFMachPortRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFStringRef = *const c_void;
    type CGEventRef = *mut c_void;
    type CGEventTapProxy = *mut c_void;
    type Callback = extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: Callback,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: CFStringRef;
        fn CFMachPortCreateRunLoopSource(
            alloc: *const c_void,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        fn CFRunLoopRun();
    }

    const SESSION_TAP: u32 = 1; // kCGSessionEventTap
    const HEAD_INSERT: u32 = 0; // kCGHeadInsertEventTap
    const DEFAULT_OPTIONS: u32 = 0; // kCGEventTapOptionDefault: may drop events
    const KEY_DOWN: u32 = 10;
    const KEY_UP: u32 = 11;
    const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const KEYCODE_FIELD: u32 = 9; // kCGKeyboardEventKeycode

    // The tap, so the callback can switch it back on if macOS disables it.
    static TAP: AtomicU64 = AtomicU64::new(0);

    extern "C" fn callback(_: CGEventTapProxy, kind: u32, event: CGEventRef, _: *mut c_void) -> CGEventRef {
        if kind == TAP_DISABLED_BY_TIMEOUT {
            let tap = TAP.load(Ordering::Acquire) as CFMachPortRef;
            // SAFETY: `tap` is the live mach port created in `start`.
            unsafe { CGEventTapEnable(tap, true) };
            return event;
        }
        if kind != KEY_DOWN && kind != KEY_UP {
            return event;
        }
        // SAFETY: `event` is the event macOS handed us for this call.
        let vk = unsafe { CGEventGetIntegerValueField(event, KEYCODE_FIELD) };
        let Ok(vk) = u16::try_from(vk) else { return event };
        let Some(up_due) = table().drop_up.get(vk as usize) else { return event };
        let drop = if kind == KEY_DOWN {
            let ours = is_trigger_box_key(vk);
            up_due.store(ours, Ordering::Release);
            ours
        } else {
            up_due.swap(false, Ordering::AcqRel)
        };
        // Returning null drops the event.
        if drop { std::ptr::null_mut() } else { event }
    }

    // SAFETY: plain permission checks with no arguments.
    let trusted = unsafe { CGPreflightPostEventAccess() || CGRequestPostEventAccess() };
    if !trusted {
        return false;
    }
    clock_zero();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    let spawned = std::thread::Builder::new().name("onset-keyfilter".into()).spawn(move || {
        let mask = (1u64 << KEY_DOWN) | (1u64 << KEY_UP);
        // SAFETY: standard event-tap setup; every pointer is either one macOS
        // just returned (checked for null) or a constant it defines.
        unsafe {
            let tap = CGEventTapCreate(
                SESSION_TAP,
                HEAD_INSERT,
                DEFAULT_OPTIONS,
                mask,
                callback,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                let _ = ready_tx.send(false);
                return;
            }
            TAP.store(tap as u64, Ordering::Release);
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if source.is_null() {
                let _ = ready_tx.send(false);
                return;
            }
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            let _ = ready_tx.send(true);
            CFRunLoopRun();
        }
    });
    spawned.is_ok() && ready_rx.recv().unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn start() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_letters_and_digits_map_to_mac_key_codes() {
        assert_eq!(mac_key_code(0x17), Some(0x11)); // t
        assert_eq!(mac_key_code(0x05), Some(0x0B)); // b
        assert_eq!(mac_key_code(0x1c), Some(0x10)); // y
        assert_eq!(mac_key_code(0x0a), Some(0x05)); // g
        assert_eq!(mac_key_code(0x15), Some(0x0F)); // r
        assert_eq!(mac_key_code(0x22), Some(0x17)); // 5
        assert_eq!(mac_key_code(0x27), Some(0x1D)); // 0
        assert_eq!(mac_key_code(0x28), None); // Enter
    }

    #[test]
    fn a_key_the_usb_reader_just_saw_is_matched_and_others_are_not() {
        saw_usb_key(0x17); // t
        assert!(is_trigger_box_key(0x11));
        // "a" was never seen: let through (after the short catch-up wait).
        assert!(!is_trigger_box_key(0x00));
    }
}
