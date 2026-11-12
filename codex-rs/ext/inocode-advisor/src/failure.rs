//! What a failure case records about a failed call: what it was doing, what it ran and what came
//! back. Only a call that really failed (non-zero exit, error text, a refusal) is a failure; a
//! stall indicator is not.

use crate::Observation;
use std::path::Path;

/// How many of the latest calls go into a failure case's conditions.
const CONDITION_ACTIONS: usize = 5;
/// The longest command text kept whole; a longer one keeps its start and its end.
const ATTEMPTED_MAX: usize = 1500;
/// The longest purpose text.
const PURPOSE_MAX: usize = 200;

/// The call as a failure signature starts with it: the tool and its arguments.
pub fn call_text(obs: &Observation) -> String {
    format!("{} {}", obs.tool, obs.args)
}

/// What the failed call said, or `None` when the call did not fail (it succeeded, or it is a
/// resumed wait). A non-zero exit with no output still says what happened.
pub fn failure_symptom(obs: &Observation) -> Option<String> {
    if obs.tool == "wait" || !obs.failed() {
        return None;
    }
    let text = obs.error.as_deref().unwrap_or("").trim();
    if !text.is_empty() {
        return Some(text.to_string());
    }
    obs.exit_code
        .filter(|c| *c != 0)
        .map(|c| format!("exit code {c}"))
}

/// The conditions of a call: the latest calls (kind and gist), the working directory, the exit
/// code, the tail of the output (the header left out) and the tools in use.
pub fn call_conditions(recent: &[String], cwd: &Path, obs: &Observation) -> String {
    let start = recent.len().saturating_sub(CONDITION_ACTIONS);
    let mut tools: Vec<&str> = Vec::new();
    for entry in recent {
        let name = entry.split_whitespace().next().unwrap_or("");
        if !name.is_empty() && !tools.contains(&name) {
            tools.push(name);
        }
    }
    format!(
        "latest calls: {}\ncwd: {}\nexit code: {}\noutput tail: {}\ntools in use: {}",
        recent[start..].join(" ; "),
        cwd.display(),
        obs.exit_code
            .map_or_else(|| "unknown".to_string(), |c| c.to_string()),
        output_tail(obs),
        tools.join(", ")
    )
}

fn output_tail(obs: &Observation) -> &str {
    obs.error
        .as_deref()
        .or(Some(obs.observation.as_str()))
        .filter(|t| !t.is_empty())
        .unwrap_or("(none)")
}

/// The command and its arguments in full (a long one keeps its start and its end), and where it
/// ran.
pub fn attempted_text(obs: &Observation, cwd: &Path) -> String {
    let command = crate::shell_command_of(&obs.args);
    let command = if command == obs.args {
        format!("{} {}", obs.tool, command)
    } else {
        format!("{}: {}", obs.tool, command)
    };
    format!("{}\ncwd: {}", middle_cut(&command, ATTEMPTED_MAX), cwd.display())
}

/// What came back: the exit code and the end of the output without the header.
pub fn outcome_text(obs: &Observation) -> String {
    let code = obs
        .exit_code
        .map_or_else(|| "unknown".to_string(), |c| c.to_string());
    format!("exit code: {code}\noutput tail: {}", output_tail(obs))
}

/// The start of the user's request for the turn.
pub fn purpose_text(request: &str) -> String {
    request.trim().chars().take(PURPOSE_MAX).collect()
}

fn middle_cut(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let half = max / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(count - half).collect();
    format!("{head}\n… [{} characters omitted] …\n{tail}", count - 2 * half)
}

/// The repository holding `cwd` and its branch, for a failure case's project: `<path> (<branch>)`.
/// Without a repository it is the folder itself. Read from `.git` directly; nothing is run.
pub fn project_text(cwd: &Path) -> String {
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        let head = if dot_git.is_dir() {
            std::fs::read_to_string(dot_git.join("HEAD")).ok()
        } else if dot_git.is_file() {
            // A worktree: `.git` is a file `gitdir: <path>`.
            std::fs::read_to_string(&dot_git)
                .ok()
                .and_then(|t| t.trim().strip_prefix("gitdir:").map(|p| p.trim().to_string()))
                .and_then(|p| std::fs::read_to_string(Path::new(&p).join("HEAD")).ok())
        } else {
            continue;
        };
        let branch = head
            .as_deref()
            .map(str::trim)
            .and_then(|h| h.strip_prefix("ref: refs/heads/"))
            .map(str::to_string)
            .or_else(|| head.as_deref().map(|h| format!("detached {}", h.trim().chars().take(12).collect::<String>())));
        return match branch {
            Some(b) => format!("{} ({b})", dir.display()),
            None => dir.display().to_string(),
        };
    }
    cwd.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(tool: &str, args: &str, exit: Option<i32>, error: Option<&str>) -> Observation {
        Observation {
            tool: tool.into(),
            args: args.into(),
            exit_code: exit,
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_call_that_failed_has_a_symptom() {
        assert_eq!(failure_symptom(&obs("exec_command", "{}", Some(0), None)), None);
        assert_eq!(
            failure_symptom(&obs("exec_command", "{}", Some(1), Some("error: no devices")))
                .as_deref(),
            Some("error: no devices")
        );
        assert_eq!(
            failure_symptom(&obs("exec_command", "{}", Some(2), None)).as_deref(),
            Some("exit code 2")
        );
        // A resumed wait is not a failure.
        assert_eq!(failure_symptom(&obs("wait", "{}", None, Some("still running"))), None);
    }

    #[test]
    fn conditions_carry_the_latest_calls_cwd_exit_code_and_output_tail() {
        let o = obs("exec_command", "{}", Some(1), Some("error: no devices/emulators found"));
        let recent: Vec<String> = [
            "exec_command a -> ok",
            "exec_command b -> ok",
            "exec_command c -> ok",
            "exec_command d -> ok",
            "read_file x -> ok",
            "exec_command adb-shell -> failed",
        ]
        .map(String::from)
        .to_vec();
        let text = call_conditions(&recent, Path::new("work"), &o);
        assert!(!text.contains("exec_command a ->"));
        assert!(text.contains("exec_command adb-shell -> failed"));
        assert!(text.contains("cwd: work"));
        assert!(text.contains("exit code: 1"));
        assert!(text.contains("output tail: error: no devices/emulators found"));
        assert!(text.contains("tools in use: exec_command, read_file"));
    }

    #[test]
    fn attempted_keeps_a_long_command_start_and_end() {
        let long = format!("{{\"cmd\":\"{}END\"}}", "x".repeat(4000));
        let text = attempted_text(&obs("exec_command", &long, Some(1), None), Path::new("w"));
        assert!(text.starts_with("exec_command: "));
        assert!(text.contains("characters omitted"));
        assert!(text.contains("END"));
        assert!(text.ends_with("cwd: w"));
        let short = attempted_text(
            &obs("exec_command", "{\"cmd\":\"adb shell\"}", Some(1), None),
            Path::new("w"),
        );
        assert_eq!(short, "exec_command: adb shell\ncwd: w");
    }

    #[test]
    fn outcome_purpose_and_project_are_plain_text() {
        let o = obs("exec_command", "{}", Some(7), Some("boom"));
        assert_eq!(outcome_text(&o), "exit code: 7\noutput tail: boom");
        assert_eq!(purpose_text(&"あ".repeat(300)).chars().count(), 200);
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        assert_eq!(
            project_text(&sub),
            format!("{} (feat/x)", dir.path().display())
        );
    }
}
