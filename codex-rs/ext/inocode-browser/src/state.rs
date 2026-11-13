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
