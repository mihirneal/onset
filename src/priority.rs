//! Ask the OS to schedule the render thread promptly.

/// Raise the calling thread's (and on Windows, the process's) priority.
/// Best effort: failures are ignored.
pub fn raise() {
    #[cfg(target_os = "macos")]
    {
        // QOS_CLASS_USER_INTERACTIVE: the class macOS uses for UI animation.
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        }
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut core::ffi::c_void;
            fn SetPriorityClass(process: *mut core::ffi::c_void, class: u32) -> i32;
            fn GetCurrentThread() -> *mut core::ffi::c_void;
            fn SetThreadPriority(thread: *mut core::ffi::c_void, priority: i32) -> i32;
        }
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn timeBeginPeriod(period_ms: u32) -> u32;
        }
        const HIGH_PRIORITY_CLASS: u32 = 0x80;
        const THREAD_PRIORITY_HIGHEST: i32 = 2;
        unsafe {
            SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
            // 1 ms scheduler granularity (default is 15.6 ms).
            timeBeginPeriod(1);
        }
    }
}
