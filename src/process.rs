//! Noninteractive child processes must not create Windows console windows.
use std::process::{Child, Command};
pub(crate) fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Do not combine with DETACHED_PROCESS: Windows ignores CREATE_NO_WINDOW then.
        command.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = command;
}
pub(crate) fn spawn_background(command: &mut Command) -> std::io::Result<Child> {
    hide_window(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Redirection alone does not prevent inheriting the original Windows pipes.
    // Restore inheritance even if spawning fails.
    #[cfg(windows)]
    let _stdio = detached_stdio::Guard::new()?;
    command.spawn()
}

#[cfg(windows)]
mod detached_stdio {
    use std::{ffi::c_void, io};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    pub(super) struct Guard(Vec<(*mut c_void, u32)>);
    impl Guard {
        pub(super) fn new() -> io::Result<Self> {
            let mut guard = Self(Vec::new());
            for kind in [-10_i32, -11, -12] {
                let handle = unsafe { GetStdHandle(kind as u32) };
                if handle.is_null() || handle as isize == -1 {
                    continue;
                }
                // Standard streams can share one handle (for example stderr redirected to stdout).
                if guard.0.iter().any(|(saved, _)| *saved == handle) {
                    continue;
                }
                let mut flags = 0;
                if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                guard.0.push((handle, flags));
                if unsafe { SetHandleInformation(handle, 1, 0) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(guard)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            for (handle, flags) in &self.0 {
                unsafe {
                    SetHandleInformation(*handle, 1, *flags & 1);
                }
            }
        }
    }
}
