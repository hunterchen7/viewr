#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::io;
use std::iter;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MB_OK: u32 = 0x0000_0000;
const MB_ICONERROR: u32 = 0x0000_0010;
const MB_SETFOREGROUND: u32 = 0x0001_0000;
const MB_TASKMODAL: u32 = 0x0000_2000;

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(window: *mut c_void, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

fn main() {
    if let Err(error) = launch_viewer() {
        show_launch_error(&error);
    }
}

fn launch_viewer() -> io::Result<()> {
    let launcher = std::env::current_exe()?;
    let install_directory = launcher
        .parent()
        .ok_or_else(|| io::Error::other("the launcher has no parent directory"))?;
    let viewer = install_directory.join("viewr.exe");

    Command::new(viewer)
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

fn show_launch_error(error: &io::Error) {
    let message = format!("Viewr could not be opened.\r\n\r\n{error}");
    let message = nul_terminated_utf16(&message);
    let caption = nul_terminated_utf16("Viewr");

    // SAFETY: Both strings are NUL-terminated and remain alive for the call.
    // A null owner is valid for a task-modal error shown during process launch.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TASKMODAL,
        );
    }
}

fn nul_terminated_utf16(value: &str) -> Vec<u16> {
    value
        .encode_utf16()
        .map(|unit| if unit == 0 { 0xfffd } else { unit })
        .chain(iter::once(0))
        .collect()
}
