//! Windows-only process-spawning helpers.
//!
//! When a detached daemon is spawned, Rust's `CreateProcessW` call runs with
//! `bInheritHandles = TRUE` (required for stdio redirection), which means the
//! child also inherits *every other* inheritable handle of the parent —
//! including a capturing shell's stdout/stderr pipes. The daemon would then
//! keep those pipes open, and `$out = forge daemon start --background` would
//! block forever waiting for EOF.
//!
//! [`InheritGuard`] temporarily clears the inheritance flag on the CLI's own
//! std handles around the spawn, so nothing leaks into the daemon.

#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

extern "system" {
    fn SetHandleInformation(h_object: *mut c_void, dw_mask: u32, dw_flags: u32) -> i32;
}

/// While this guard is alive, the process's std handles are marked
/// non-inheritable. The original state is restored on drop.
pub struct InheritGuard {
    handles: Vec<(*mut c_void, bool)>,
}

impl InheritGuard {
    pub fn suppress_std_handles() -> Self {
        let mut handles = vec![];
        let std_handles: [*mut c_void; 3] = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        for handle in std_handles {
            if handle.is_null() {
                continue;
            }
            let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0;
            handles.push((handle, ok));
        }
        Self { handles }
    }
}

impl Drop for InheritGuard {
    fn drop(&mut self) {
        for (handle, was_settable) in &self.handles {
            if *was_settable {
                unsafe {
                    SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
                }
            }
        }
    }
}
