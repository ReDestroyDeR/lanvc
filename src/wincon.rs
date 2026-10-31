//! Console handling for the GUI-subsystem Windows build.
//!
//! The exe is linked with SUBSYSTEM:windows so double-clicking it pops no
//! terminal. Output must still survive:
//! - launched from a console (selftest/headless) -> attach to it and print
//!   there;
//! - launched with no console (double-click) -> stdout/stderr go to
//!   %TEMP%\lanvc.log, which also captures panics.
//!
//! Must run before anything touches stdout/stderr (Rust caches the std
//! handles on first use).

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;

const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF; // (DWORD)-1
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const OPEN_EXISTING: u32 = 3;

#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(dw_process_id: u32) -> i32;
    fn GetConsoleWindow() -> *mut c_void;
    fn SetStdHandle(n_std_handle: u32, h_handle: *mut c_void) -> i32;
    fn CreateFileW(
        lp_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *mut c_void,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: *mut c_void,
    ) -> *mut c_void;
}

pub fn setup() {
    unsafe {
        // Own console already (launched from cmd/PowerShell)? Keep whatever
        // the launcher set up (also covers `lanvc.exe --selftest > file`).
        if !GetConsoleWindow().is_null() {
            return;
        }
        // Started from a shell that has a console but didn't give it to us
        // (GUI subsystem): attach to the parent's console.
        if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
            let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
            let con = CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            );
            if !con.is_null() && con as isize != -1 {
                SetStdHandle(STD_OUTPUT_HANDLE, con);
                SetStdHandle(STD_ERROR_HANDLE, con);
            }
            return;
        }
        // No console anywhere: log to %TEMP%\lanvc.log.
        redirect_to_logfile();
    }
}

fn redirect_to_logfile() {
    let path = std::env::temp_dir().join("lanvc.log");
    let Ok(f) = std::fs::File::create(&path) else {
        return;
    };
    let h = f.as_raw_handle() as *mut c_void;
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, h);
        SetStdHandle(STD_ERROR_HANDLE, h);
    }
    // The handle must outlive this function: keep the File alive forever
    // (one per process, closed by the OS at exit).
    std::mem::forget(f);
}
