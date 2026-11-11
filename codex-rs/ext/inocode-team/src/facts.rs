//! Facts the host observed about a child agent, handed to the parent next to the child's own
//! report. Only what the host recorded: files changed through edit tools, commands run through
//! shell tools with their exit codes, and how long it took. Nothing is summarised or paraphrased
//! by a model, and what the host cannot tell is written as unknown.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// One command the child ran.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandFact {
    pub command: String,
    /// `None` when the output did not carry an exit code.
    pub exit_code: Option<i32>,
    pub secs: f64,
}

/// What was recorded for one child since the last report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// Milliseconds since the epoch when the child (or its current turn) started.
    pub started_ms: Option<u64>,
    /// Paths of files changed through edit tools, in order, without repeats.
    pub changed_files: Vec<String>,
    /// Edits whose tool call did not name a path.
    pub unnamed_edits: usize,
    /// Every command, in order.
    pub commands: Vec<CommandFact>,
    /// How many commands had run when the last file change happened.
    pub commands_at_last_change: usize,
}

fn store() -> &'static Mutex<HashMap<String, Facts>> {
    static S: OnceLock<Mutex<HashMap<String, Facts>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Facts>> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Starts recording for a child thread. Calls for threads that were never noted are ignored, so
/// only spawned children are tracked.
pub fn note_spawned(thread_id: &str) {
    lock().insert(
        thread_id.to_string(),
        Facts {
            started_ms: Some(now_ms()),
            ..Facts::default()
        },
    );
}

/// A file-changing tool call of a tracked child succeeded.
pub fn record_edit(thread_id: &str, paths: Vec<String>) {
    let mut map = lock();
    let Some(f) = map.get_mut(thread_id) else {
        return;
    };
    if paths.is_empty() {
        f.unnamed_edits += 1;
    }
    for p in paths {
        if !f.changed_files.contains(&p) {
            f.changed_files.push(p);
        }
    }
    f.commands_at_last_change = f.commands.len();
}

/// A shell tool call of a tracked child finished.
pub fn record_command(thread_id: &str, command: &str, exit_code: Option<i32>, secs: f64) {
    let mut map = lock();
    let Some(f) = map.get_mut(thread_id) else {
        return;
    };
    f.commands.push(CommandFact {
        command: command.to_string(),
        exit_code,
        secs,
    });
}

/// The block to append to the child's result, and a fresh start for its next turn. `None` when
/// the thread is not tracked.
pub fn take_report(thread_id: &str) -> Option<String> {
    let mut map = lock();
    let f = map.get_mut(thread_id)?;
    let elapsed = f.started_ms.map(|s| now_ms().saturating_sub(s) as f64 / 1000.0);
    let text = render(f, elapsed);
    *f = Facts {
        started_ms: Some(now_ms()),
        ..Facts::default()
    };
    Some(text)
}

fn clip(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Renders the facts. Plain statements, no judgement.
pub fn render(f: &Facts, elapsed_secs: Option<f64>) -> String {
    let mut out = String::from("[host-recorded facts; not written by the agent]\n");
    match elapsed_secs {
        Some(s) => out.push_str(&format!("elapsed: {s:.0} s\n")),
        None => out.push_str("elapsed: unknown\n"),
    }
    if f.changed_files.is_empty() && f.unnamed_edits == 0 {
        out.push_str("files changed through edit tools: none\n");
    } else {
        out.push_str(&format!(
            "files changed through edit tools ({}): {}",
            f.changed_files.len(),
            f.changed_files.join(", ")
        ));
        if f.unnamed_edits > 0 {
            if !f.changed_files.is_empty() {
                out.push_str("; ");
            }
            out.push_str(&format!("{} edit(s) with no path recorded", f.unnamed_edits));
        }
        out.push('\n');
    }
    let after = &f.commands[f.commands_at_last_change.min(f.commands.len())..];
    let any_change = !f.changed_files.is_empty() || f.unnamed_edits > 0;
    if any_change {
        out.push_str(&format!("commands run after the last change ({}):\n", after.len()));
    } else {
        out.push_str(&format!("commands run ({}):\n", after.len()));
    }
    for c in after {
        let exit = c
            .exit_code
            .map_or_else(|| "exit unknown".to_string(), |code| format!("exit {code}"));
        out.push_str(&format!("- `{}` -> {exit}, {:.1} s\n", clip(&c.command, 200), c.secs));
    }
    if any_change && f.commands_at_last_change > 0 {
        out.push_str(&format!(
            "commands run before the last change: {}\n",
            f.commands_at_last_change
        ));
    }
    out.push_str("changes made through shell commands are not tracked.");
    out
}

/// Paths a file-changing tool call names: the `*** Add/Update/Delete File:` lines of a patch and
/// any `path` / `file_path` / `filename` string of a JSON call.
pub fn edited_paths(args: &str) -> Vec<String> {
    fn push(paths: &mut Vec<String>, p: &str) {
        let p = p.trim();
        if !p.is_empty() && !paths.iter().any(|q| q == p) {
            paths.push(p.to_string());
        }
    }
    let mut paths: Vec<String> = Vec::new();
    for line in args.lines() {
        let line = line.trim();
        for marker in ["*** Add File:", "*** Update File:", "*** Delete File:"] {
            if let Some(rest) = line.strip_prefix(marker) {
                push(&mut paths, rest);
            }
        }
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["path", "file_path", "filename", "file"] {
            if let Some(serde_json::Value::String(p)) = map.get(key) {
                push(&mut paths, p);
            }
        }
    }
    // A patch inside a JSON string keeps its markers as escaped text.
    if paths.is_empty() {
        for marker in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
            let mut rest = args;
            while let Some(pos) = rest.find(marker) {
                let after = &rest[pos + marker.len()..];
                let mut end = after.find(['\n', '"']).unwrap_or(after.len());
                if let Some(escaped) = after[..end].find("\\n") {
                    end = escaped;
                }
                push(&mut paths, &after[..end]);
                rest = &after[end..];
            }
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_come_from_patches_and_json_calls() {
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n*** Add File: docs/a.md\n*** End Patch";
        assert_eq!(edited_paths(patch), ["src/lib.rs", "docs/a.md"]);
        assert_eq!(edited_paths(r#"{"file_path":"a/b.py","x":1}"#), ["a/b.py"]);
        let escaped = "{\"input\":\"*** Begin Patch\\n*** Update File: a/b.rs\\n@@\\n*** End Patch\"}";
        assert_eq!(edited_paths(escaped), ["a/b.rs"]);
        assert!(edited_paths("{}").is_empty());
    }

    #[test]
    fn only_spawned_children_are_tracked() {
        record_edit("facts-nobody", vec!["a.rs".into()]);
        record_command("facts-nobody", "ls", Some(0), 0.1);
        assert!(take_report("facts-nobody").is_none());
    }

    #[test]
    fn the_report_lists_changes_and_the_commands_after_the_last_one() {
        let id = "facts-child-1";
        note_spawned(id);
        record_command(id, "cargo check", Some(1), 3.0);
        record_edit(id, vec!["src/a.rs".into(), "src/b.rs".into()]);
        record_edit(id, vec!["src/a.rs".into()]);
        record_command(id, "cargo test -p x", Some(0), 12.34);
        record_command(id, "ls", None, 0.05);
        let report = take_report(id).expect("report");
        assert!(report.starts_with("[host-recorded facts; not written by the agent]"));
        assert!(report.contains("files changed through edit tools (2): src/a.rs, src/b.rs"));
        assert!(report.contains("commands run after the last change (2):"));
        assert!(report.contains("- `cargo test -p x` -> exit 0, 12.3 s"));
        assert!(report.contains("- `ls` -> exit unknown, 0.1 s"));
        assert!(report.contains("commands run before the last change: 1"));
        assert!(report.ends_with("changes made through shell commands are not tracked."));
        // The next turn starts clean.
        let next = take_report(id).expect("report");
        assert!(next.contains("files changed through edit tools: none"));
        assert!(next.contains("commands run (0):"));
    }

    #[test]
    fn nothing_changed_lists_every_command_and_unknowns_stay_unknown() {
        let f = Facts {
            commands: vec![CommandFact {
                command: "python   a.py\n--flag".into(),
                exit_code: None,
                secs: 1.0,
            }],
            ..Facts::default()
        };
        let text = render(&f, None);
        assert!(text.contains("elapsed: unknown"));
        assert!(text.contains("files changed through edit tools: none"));
        assert!(text.contains("commands run (1):"));
        assert!(text.contains("- `python a.py --flag` -> exit unknown, 1.0 s"));
    }

    #[test]
    fn an_edit_without_a_path_is_said_so() {
        let f = Facts {
            unnamed_edits: 1,
            ..Facts::default()
        };
        assert!(render(&f, Some(5.0)).contains("1 edit(s) with no path recorded"));
    }
}
