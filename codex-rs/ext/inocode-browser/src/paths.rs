//! Where everything lives. All of it is under INOCODE_HOME (default ~/.inocode); the user's own
//! Chrome profile is never touched (65 B-6).
use std::path::PathBuf;

pub fn inocode_home() -> PathBuf {
    if let Some(h) = std::env::var_os("INOCODE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    PathBuf::from(std::env::var_os(user).unwrap_or_default()).join(".inocode")
}

pub fn browser_dir() -> PathBuf {
    inocode_home().join("browser")
}

/// Runtime CEF files (one directory per CEF version).
pub fn cef_root() -> PathBuf {
    browser_dir().join("cef")
}

/// InoCode-only profile (cookies, cache, local storage).
pub fn profile_dir() -> PathBuf {
    browser_dir().join("profile")
}

pub fn log_dir() -> PathBuf {
    browser_dir().join("logs")
}

/// `{"pid":..,"port":..}` of the running browser; how MCP wrappers and the GUI find the CDP port.
pub fn state_file() -> PathBuf {
    browser_dir().join("cdp.json")
}
