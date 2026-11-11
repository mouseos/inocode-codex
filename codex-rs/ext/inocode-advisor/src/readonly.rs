//! Which tool calls only look: they neither change files nor tests, and an investigation made of
//! them is not a stall. Used so that "no progress" does not count research.

/// Shell programs that only read.
const READ_PROGRAMS: &[&str] = &[
    "ls", "dir", "cat", "type", "head", "tail", "rg", "grep", "egrep", "find", "pwd", "echo", "wc",
    "which", "where", "tree", "stat", "file", "get-content", "get-childitem", "select-string",
    "get-item", "test-path", "get-location", "get-command", "get-process", "sort", "diff",
    "findstr", "gc", "get-date", "select-object", "where-object", "foreach-object",
    "measure-object", "sort-object", "format-table", "out-string", "convertfrom-json",
    "convertto-json", "invoke-webrequest", "invoke-restmethod",
];

/// `git` sub-commands that only read.
const READ_GIT: [&str; 10] = [
    "status", "log", "diff", "show", "rev-parse", "ls-files", "blame", "describe", "ls-tree",
    "cat-file",
];

fn first_words(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '(' | ')' | '&' | '@'))
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The command inside `<path to a shell> ... "<command>"`, when the first word names a shell by
/// a path (`"C:\Windows\...\powershell.exe" -Command "..."`).
fn inner_of_shell_path(text: &str) -> Option<String> {
    let first = if let Some(rest) = text.strip_prefix('"') {
        rest.split('"').next()?
    } else {
        text.split_whitespace().next()?
    };
    if !first.contains(['\\', '/']) {
        return None;
    }
    let file = first.rsplit(['\\', '/']).next()?.to_lowercase();
    let stem = file.strip_suffix(".exe").unwrap_or(&file);
    if !["powershell", "pwsh", "cmd", "bash", "sh"].contains(&stem) {
        return None;
    }
    let pos = text.find(first)? + first.len();
    let rest = text[pos..].strip_prefix('"').unwrap_or(&text[pos..]);
    let quote = rest.find(['"', '\''])?;
    Some(rest[quote + 1..].trim_end_matches(['"', '\'']).to_string())
}


/// A command line is read-only when every pipeline segment starts with a reading program and
/// nothing is redirected. Anything unrecognised is not read-only.
pub fn command_is_read_only(command: &str) -> bool {
    let mut text = command.trim().to_string();
    // `"C:\...\powershell.exe" -Command "..."`: the shell may be named by its path.
    if let Some(inner) = inner_of_shell_path(&text) {
        text = inner;
    }
    // `powershell -NoProfile -Command "..."`: look at the inner command.
    let lower = text.to_lowercase();
    for shell in ["powershell", "pwsh", "bash -lc", "bash -c", "sh -c", "cmd /c"] {
        if lower.starts_with(shell)
            && let Some(pos) = lower.find(['"', '\''])
        {
            text = text[pos + 1..].trim_end_matches(['"', '\'']).to_string();
            break;
        }
    }
    if text.contains('>') || text.to_lowercase().contains("tee ") {
        return false;
    }
    let mut any = false;
    for segment in text.split(['|', ';', '\n']).flat_map(|s| s.split("&&")) {
        let words = first_words(segment);
        let Some(first) = words.first() else {
            continue;
        };
        any = true;
        let ok = if first == "git" {
            // `git -C dir log`: the sub-command is the first word that is not an option or the
            // value of `-C` / `-c`.
            let mut sub = None;
            let mut skip = false;
            for w in words.iter().skip(1) {
                if skip {
                    skip = false;
                    continue;
                }
                if w == "-c" {
                    skip = true;
                } else if !w.starts_with('-') {
                    sub = Some(w.as_str());
                    break;
                }
            }
            sub.is_some_and(|sub| READ_GIT.contains(&sub))
        } else {
            READ_PROGRAMS.contains(&first.as_str())
        };
        if !ok {
            return false;
        }
    }
    any
}

/// The command a shell-like tool call carries: a JSON `cmd` / `command` string, else the text.
pub fn shell_command_of(args: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(args) {
        for key in ["cmd", "command"] {
            if let Some(serde_json::Value::String(s)) = map.get(key) {
                return s.clone();
            }
        }
    }
    args.to_string()
}

/// True when the call only looks at things. Tools named for reading, listing, searching or
/// viewing are read-only; a shell tool is read-only when its command is.
pub fn is_read_only_call(tool: &str, args: &str) -> bool {
    let name = tool.to_lowercase();
    // Waiting on a running code-mode cell: the work happens elsewhere.
    if name == "wait" {
        return true;
    }
    if name.contains("exec") || name.contains("shell") || name.contains("bash") || name.contains("terminal")
    {
        let command = shell_command_of(args);
        if let Some(read_only) = code_mode_is_read_only(&command) {
            return read_only;
        }
        return command_is_read_only(&command);
    }
    tool_name_is_read_only(&name)
}

/// Tools named for looking, plus the ones that hand work to other agents: starting or briefing
/// an agent is the work of an orchestrator, not a stall.
fn tool_name_is_read_only(name: &str) -> bool {
    if ["spawn_agent", "send_input", "wait_agent"].iter().any(|w| name.contains(w)) {
        return true;
    }
    ["read", "view", "list", "grep", "glob", "search", "fetch", "get_goal", "find"]
        .iter()
        .any(|w| name.contains(w))
        && !name.contains("write")
}

/// True when the text is a code-mode `exec` script (JavaScript calling `tools.<name>(...)`)
/// rather than a shell command line.
pub fn is_code_mode_script(text: &str) -> bool {
    text.contains("tools.") || text.contains("ALL_TOOLS")
}

/// A code-mode `exec` script (JavaScript calling `tools.<name>(...)`): read-only when every tool
/// it calls is. `None` when the text does not look like such a script.
fn code_mode_is_read_only(source: &str) -> Option<bool> {
    if !source.contains("tools.") && !source.contains("ALL_TOOLS") {
        return None;
    }
    let mut rest = source;
    while let Some(pos) = rest.find("tools.") {
        let after = &rest[pos + "tools.".len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = after[..end].to_lowercase();
        if !name.is_empty() && !tool_name_is_read_only(&name) {
            return Some(false);
        }
        rest = after;
    }
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looking_commands_are_read_only() {
        for cmd in [
            "ls -la src",
            "rg -n foo src | head -20",
            "git status",
            "git -C repo log --oneline",
            "git diff HEAD~1",
            "Get-Content -LiteralPath a.txt",
            "powershell -NoProfile -Command \"Get-ChildItem . | Select-String foo\"",
            "cat a.rs && wc -l a.rs",
        ] {
            assert!(command_is_read_only(cmd), "{cmd}");
        }
    }

    #[test]
    fn anything_that_runs_or_writes_is_not() {
        for cmd in [
            "cargo test",
            "git commit -m x",
            "git push",
            "echo hi > a.txt",
            "rg foo | tee out.txt",
            "python build.py",
            "ls; rm -rf x",
            "",
        ] {
            assert!(!command_is_read_only(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_shell_named_by_its_path_is_looked_through() {
        let read = "\"C:\\\\WINDOWS\\\\System32\\\\WindowsPowerShell\\\\v1.0\\\\powershell.exe\" -Command \"Get-Content -Raw 'C:\\\\a\\\\SKILL.md'\"";
        assert!(command_is_read_only(read), "{read}");
        let date = "\"C:\\\\WINDOWS\\\\System32\\\\WindowsPowerShell\\\\v1.0\\\\powershell.exe\" -Command \"Get-Date -Format 'yyyy'\"";
        assert!(command_is_read_only(date), "{date}");
        let write = "\"C:\\\\WINDOWS\\\\System32\\\\WindowsPowerShell\\\\v1.0\\\\powershell.exe\" -Command \"Remove-Item x\"";
        assert!(!command_is_read_only(write), "{write}");
    }

    #[test]
    fn a_research_exec_script_is_read_only_and_a_writing_one_is_not() {
        let look = "const xs = ALL_TOOLS.filter(x => /agent/.test(x.name));\ntext(xs);";
        assert!(is_read_only_call("exec", look));
        let shop = "const r = await tools.shopping_search({provider:'mercari', query:'x'}); text(r);";
        assert!(is_read_only_call("exec", shop));
        let web = "await Promise.all(q.map(x => tools.web_search({query: x})));";
        assert!(is_read_only_call("exec", web));
        let edit = "await tools.apply_patch({input: p});";
        assert!(!is_read_only_call("exec", edit));
        let mixed = "await tools.web_search({query:'a'}); await tools.apply_patch({input: p});";
        assert!(!is_read_only_call("exec", mixed));
    }

    #[test]
    fn handing_work_to_an_agent_is_not_counted_as_standing_still() {
        assert!(is_read_only_call("multi_agent_v1spawn_agent", "{}"));
        assert!(is_read_only_call("multi_agent_v1send_input", "{}"));
        assert!(is_read_only_call("multi_agent_v1wait_agent", "{}"));
        assert!(is_read_only_call("wait", "{\"cell_id\":\"3\"}"));
    }

    #[test]
    fn tools_are_classified_by_name_and_command() {
        assert!(is_read_only_call("read_file", "{\"path\":\"a\"}"));
        assert!(is_read_only_call("grep_files", "{}"));
        assert!(is_read_only_call("exec_command", "{\"cmd\":\"rg foo\"}"));
        assert!(!is_read_only_call("exec_command", "{\"cmd\":\"cargo check\"}"));
        assert!(!is_read_only_call("apply_patch", "*** Begin Patch"));
        assert!(!is_read_only_call("write_file", "{}"));
        assert!(is_read_only_call("wait_agent", "{}"));
    }
}
