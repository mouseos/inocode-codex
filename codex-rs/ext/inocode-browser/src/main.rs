//! `inocode-browser`: the InoCode browser tab host (65). A separate executable that runs only while
//! the tab is open.
//!
//!   inocode-browser [run] [--url URL] [--port N] [--parent-pid PID] [--parent-window HWND] [--stdin-control]
//!       --parent-window: live as a child window of that window (the GUI's tab; placed by the GUI).
//!       --stdin-control: close when standard input says `close` or ends.
//!       Fetch CEF on first use, open one window, serve CDP on 127.0.0.1:<random port>, print one
//!       JSON line `{"event":"ready","pid":..,"port":..,"endpoint":..}` on stdout, and exit (with
//!       every CEF process) when the window is closed or --parent-pid dies.
//!   inocode-browser fetch            download/verify/unpack CEF only
//!   inocode-browser status           print the running browser's endpoint (exit 1 if none)
//!   inocode-browser close            close the running browser's window (every process ends)
//!   inocode-browser mcp-config [--write]
//!       print (or append to INOCODE_HOME/config.toml) the [mcp_servers.*] tables
//!   inocode-browser mcp playwright|devtools     MCP server bridge (used by the config entries)
//!
//! Windows is the only verified platform; macOS and Linux are not verified.
mod cdp;
mod fetch;
mod host;
mod mcp;
mod paths;
mod state;

use std::process::ExitCode;

fn flag(args: &[String], name: &str) -> Option<String> {
    let eq = format!("{name}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&eq) {
            return Some(v.to_string());
        }
    }
    None
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("inocode-browser: {msg}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // CEF sub-process (renderer, GPU, ...): the parent put the CEF directory in the environment.
    if args.iter().any(|a| a.starts_with("--type=")) {
        let Some(dir) = host::child_cef_dir() else { return fail("sub-process without CEF directory") };
        host::prepare_library(&dir);
        return match host::run(None) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(e),
        };
    }

    let exe = std::env::current_exe().unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("fetch") => match fetch::ensure() {
            Ok(dir) => {
                println!("{}", dir.display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(e),
        },
        Some("status") => match state::read_live() {
            Some(ep) => {
                println!("{}", status_json(ep));
                ExitCode::SUCCESS
            }
            None => ExitCode::from(1),
        },
        Some("close") => match state::read_live() {
            Some(ep) if state::close_windows(ep.pid) > 0 => ExitCode::SUCCESS,
            Some(_) => fail("no window to close"),
            None => ExitCode::SUCCESS,
        },
        Some("mcp-config") => {
            if args.iter().any(|a| a == "--write") {
                match mcp::write_config(&exe) {
                    Ok(added) => {
                        println!("added: {}", if added.is_empty() { "(nothing; all present)".into() } else { added.join(", ") });
                        ExitCode::SUCCESS
                    }
                    Err(e) => fail(e),
                }
            } else {
                for (_, body) in mcp::config_snippet(&exe) {
                    println!("{body}");
                }
                ExitCode::SUCCESS
            }
        }
        Some("mcp") => match mcp::serve(args.get(1).map(String::as_str).unwrap_or(""), &exe) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => fail(e),
        },
        _ => run(&args),
    }
}

fn status_json(ep: state::Endpoint) -> String {
    serde_json::json!({
        "event": "ready",
        "pid": ep.pid,
        "port": ep.port,
        "endpoint": format!("http://127.0.0.1:{}", ep.port),
    })
    .to_string()
}

fn run(args: &[String]) -> ExitCode {
    // One browser at a time: hand back the one that is already up (and open --url in it).
    if let Some(ep) = state::read_live() {
        if let Some(url) = flag(args, "--url") {
            if let Err(e) = cdp::navigate(ep.port, &url) {
                return fail(format!("cannot open {url}: {e}"));
            }
        }
        println!("{}", status_json(ep));
        return ExitCode::SUCCESS;
    }
    let cef_dir = match fetch::ensure() {
        Ok(d) => d,
        Err(e) => return fail(format!("cannot get CEF: {e}")),
    };
    host::prepare_library(&cef_dir);

    // Chromium looks for its helper DLLs next to the executable, so the real browser runs from a
    // copy of this exe placed inside the fetched CEF directory. This process just waits for it.
    // The copy is named after this build (size and time), so a browser still running from an older
    // copy does not stop a newer build from starting; older copies are removed when not in use.
    let exe = std::env::current_exe().unwrap_or_default();
    if exe.parent() != Some(cef_dir.as_path()) {
        let meta = std::fs::metadata(&exe).ok();
        let stamp = meta
            .as_ref()
            .map(|m| {
                let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                format!("{}-{t}", m.len())
            })
            .unwrap_or_default();
        let stem = exe.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "inocode-browser".into());
        let ext = exe.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let inner = cef_dir.join(format!("{stem}-{stamp}{ext}"));
        if !inner.is_file() {
            if let Err(e) = std::fs::copy(&exe, &inner) {
                return fail(format!("cannot place host in {}: {e}", cef_dir.display()));
            }
        }
        if let Ok(entries) = std::fs::read_dir(&cef_dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with(&stem) && n.ends_with(&ext) && e.path() != inner {
                    let _ = std::fs::remove_file(e.path()); // fails while it runs: kept
                }
            }
        }
        let mut cmd = std::process::Command::new(&inner);
        cmd.args(args);
        if flag(args, "--parent-pid").is_none() {
            cmd.arg(format!("--parent-pid={}", std::process::id()));
        }
        return match cmd.status() {
            Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
            Err(e) => fail(e),
        };
    }

    let port = match flag(args, "--port").and_then(|p| p.parse::<u16>().ok()) {
        Some(p) => p,
        None => state::pick_port(),
    };
    if port < 1024 {
        return fail("no usable port");
    }
    let me = std::process::id();
    let options = host::Options {
        url: flag(args, "--url").unwrap_or_else(|| "about:blank".into()),
        port,
        cef_dir,
        profile: paths::profile_dir(),
        log_file: paths::log_dir().join("cef.log"),
        parent_pid: flag(args, "--parent-pid").and_then(|p| p.parse().ok()),
        parent_window: flag(args, "--parent-window").and_then(|p| p.parse().ok()),
        stdin_control: args.iter().any(|a| a == "--stdin-control"),
    };

    // Announce the endpoint once the port really answers.
    std::thread::spawn(move || {
        for _ in 0..600 {
            if state::port_open(port) {
                let ep = state::Endpoint { pid: me, port };
                state::write(ep);
                println!("{}", status_json(ep));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    let result = host::run(Some(options));
    state::remove_if_mine(me);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}
