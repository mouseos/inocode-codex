//! MCP registration (65 B-4, B-8) and the launch-on-demand bridge.
//!
//! The CDP port is random per launch, but an MCP server's arguments are fixed at registration. So
//! `inocode-browser mcp <kind>` runs the real MCP server (Playwright MCP / chrome-devtools-mcp)
//! against a stable loopback port that it owns, and forwards each connection to the browser that
//! is running *now* - starting `inocode-browser` on the first connection if none is. A session
//! that never uses the browser never opens a window.
use crate::{paths, state};
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn npx() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}

/// `[mcp_servers.*]` tables for `~/.inocode/config.toml`. Same shape as the agent-desktop entry.
pub fn config_snippet(exe: &Path) -> Vec<(String, String)> {
    let exe = exe.display();
    let t = |name: &str, command: &str, args: &str, enabled: bool| -> (String, String) {
        let mut s = format!("[mcp_servers.{name}]\ncommand = '{command}'\nargs = {args}\n");
        if !enabled {
            s.push_str("enabled = false\n");
        }
        s.push_str("startup_timeout_sec = 60\ntool_timeout_sec = 120\n");
        (format!("[mcp_servers.{name}]"), s)
    };
    vec![
        // Default: Playwright MCP on the InoCode browser tab.
        t("browser", &exe.to_string(), r#"["mcp", "playwright"]"#, true),
        // Alternative to the above (same browser, Google's DevTools MCP). Enable one or both.
        t("browser-devtools", &exe.to_string(), r#"["mcp", "devtools"]"#, false),
        // Separate browser for Firefox-only work; not part of the tab (65 8). Never started unless enabled.
        t(
            "browser-firefox",
            npx(),
            r#"["-y", "@playwright/mcp@latest", "--browser", "firefox"]"#,
            false,
        ),
    ]
}

/// Append the entries that are not in `config.toml` yet. Existing tables are never touched.
pub fn write_config(exe: &Path) -> Res<Vec<String>> {
    let path = paths::inocode_home().join("config.toml");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut added = Vec::new();
    for (header, body) in config_snippet(exe) {
        if text.contains(&header) {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&body);
        added.push(header);
    }
    if !added.is_empty() {
        std::fs::create_dir_all(paths::inocode_home())?;
        std::fs::write(&path, text)?;
    }
    Ok(added)
}

/// The browser that is running now; launch one if there is none.
fn backend(exe: &Path) -> Res<u16> {
    if let Some(ep) = state::read_live() {
        return Ok(ep.port);
    }
    Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let end = Instant::now() + Duration::from_secs(600); // first launch may download CEF
    while Instant::now() < end {
        if let Some(ep) = state::read_live() {
            return Ok(ep.port);
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err("browser did not start".into())
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.flush();
    let _ = to.shutdown(std::net::Shutdown::Both);
    let _ = from.shutdown(std::net::Shutdown::Both);
}

fn forward(client: TcpStream, exe: &Path) {
    let Ok(port) = backend(exe) else { return };
    let Ok(server) = TcpStream::connect(("127.0.0.1", port)) else { return };
    let (Ok(c2), Ok(s2)) = (client.try_clone(), server.try_clone()) else { return };
    thread::spawn(move || pipe(client, server));
    pipe(s2, c2);
}

fn kill_tree(child: &mut Child) {
    if cfg!(windows) {
        // Our own child only (by PID), with its children: npx.cmd -> node.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else {
        let _ = child.kill();
    }
}

/// `inocode-browser mcp playwright|devtools`: stdio is the MCP channel and goes to the child.
pub fn serve(kind: &str, exe: &Path) -> Res<i32> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let args: Vec<String> = match kind {
        "playwright" => vec!["-y".into(), "@playwright/mcp@latest".into(), "--cdp-endpoint".into(), endpoint],
        "devtools" => vec![
            "-y".into(),
            "chrome-devtools-mcp@latest".into(),
            format!("--browser-url={endpoint}"),
            "--no-usage-statistics".into(),
        ],
        other => return Err(format!("unknown mcp kind '{other}' (playwright | devtools)").into()),
    };
    let exe_owned = exe.to_path_buf();
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let exe = exe_owned.clone();
            thread::spawn(move || forward(conn, &exe));
        }
    });
    let mut child = Command::new(npx()).args(&args).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()?;
    let status = child.wait();
    kill_tree(&mut child);
    Ok(status.ok().and_then(|s| s.code()).unwrap_or(1))
}
