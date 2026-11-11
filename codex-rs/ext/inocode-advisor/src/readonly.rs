//! Which tool calls only look: they neither change files nor tests, and an investigation made of
//! them is not a stall. Used so that "no progress" does not count research.

/// Shell programs that only read.
const READ_PROGRAMS: [&str; 30] = [
    "ls", "dir", "cat", "type", "head", "tail", "rg", "grep", "egrep", "find", "pwd", "echo", "wc",
    "which", "where", "tree", "stat", "file", "get-content", "get-childitem", "select-string",
    "get-item", "test-path", "get-location", "get-command", "get-process", "sort", "diff",
    "findstr", "gc",
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

/// A command line is read-only when every pipeline segment starts with a reading program and
/// nothing is redirected. Anything unrecognised is not read-only.
pub fn command_is_read_only(command: &str) -> bool {
    let mut text = command.trim().to_string();
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
    if name.contains("exec") || name.contains("shell") || name.contains("bash") || name.contains("terminal")
    {
        return command_is_read_only(&shell_command_of(args));
    }
    ["read", "view", "list", "grep", "glob", "search", "fetch", "get_goal", "find"]
        .iter()
        .any(|w| name.contains(w))
        && !name.contains("write")
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
    fn tools_are_classified_by_name_and_command() {
        assert!(is_read_only_call("read_file", "{\"path\":\"a\"}"));
        assert!(is_read_only_call("grep_files", "{}"));
        assert!(is_read_only_call("exec_command", "{\"cmd\":\"rg foo\"}"));
        assert!(!is_read_only_call("exec_command", "{\"cmd\":\"cargo check\"}"));
        assert!(!is_read_only_call("apply_patch", "*** Begin Patch"));
        assert!(!is_read_only_call("write_file", "{}"));
        assert!(!is_read_only_call("wait_agent", "{}"));
    }
}
