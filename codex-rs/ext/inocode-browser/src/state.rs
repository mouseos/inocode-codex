use crate::paths;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
}

pub fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// The running browser, if its CDP port still answers.
pub fn read_live() -> Option<Endpoint> {
    let text = std::fs::read_to_string(paths::state_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let ep = Endpoint {
        pid: v.get("pid")?.as_u64()? as u32,
        port: v.get("port")?.as_u64()? as u16,
    };
    port_open(ep.port).then_some(ep)
}

pub fn write(ep: Endpoint) {
    let _ = std::fs::create_dir_all(paths::browser_dir());
    let body = serde_json::json!({ "pid": ep.pid, "port": ep.port });
    let _ = std::fs::write(paths::state_file(), body.to_string());
}

/// Remove the state file only if it still describes this process.
pub fn remove_if_mine(pid: u32) {
    if let Ok(text) = std::fs::read_to_string(paths::state_file()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if v.get("pid").and_then(|p| p.as_u64()) == Some(pid as u64) {
                let _ = std::fs::remove_file(paths::state_file());
            }
        }
    }
}

/// A free loopback port, different on every launch (65 B-5).
pub fn pick_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(0)
}

/// Close the running browser the way its close button does: WM_CLOSE to its top-level windows.
/// Returns how many windows were asked to close.
#[cfg(windows)]
pub fn close_windows(pid: u32) -> usize {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
    };
    struct Seek {
        pid: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let seek = unsafe { &mut *(lparam as *mut Seek) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut owner) };
        if owner == seek.pid && unsafe { IsWindowVisible(hwnd) } != 0 {
            seek.found.push(hwnd);
        }
        1
    }
    let mut seek = Seek { pid, found: Vec::new() };
    unsafe { EnumWindows(Some(each), &mut seek as *mut Seek as LPARAM) };
    for &hwnd in &seek.found {
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    }
    seek.found.len()
}

#[cfg(not(windows))]
pub fn close_windows(pid: u32) -> usize {
    // SIGTERM: the CEF message loop ends and every process with it (unverified off Windows).
    let ok = std::process::Command::new("kill").arg(pid.to_string()).status().is_ok_and(|s| s.success());
    usize::from(ok)
}
