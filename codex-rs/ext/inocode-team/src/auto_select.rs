//! `team.auto_select`: look at a user request, and when it contains several independent
//! pieces of work, estimate the speed-up and produce a recommendation for the main model.

use crate::estimate::Estimate;
use crate::estimate::EstimateConfig;
use crate::estimate::Task;
use crate::estimate::estimate;
use crate::estimate::recommendation_text;

/// Default expected duration of one detected task, seconds.
pub const DEFAULT_TASK_SECONDS: f64 = 600.0;

fn list_item(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for p in ["- ", "* ", "・", "• "] {
        if let Some(rest) = t.strip_prefix(p) {
            return Some(rest.trim());
        }
    }
    // "1. foo", "2) foo", "①foo"
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        for p in [". ", ") ", "、", "."] {
            if let Some(r) = rest.strip_prefix(p) {
                return Some(r.trim());
            }
        }
    }
    let mut chars = t.chars();
    if let Some(c) = chars.next()
        && ('①'..='⑳').contains(&c)
    {
        return Some(chars.as_str().trim());
    }
    None
}

/// A list item that is a knowledge record, e.g. `[rule-0005] ...` from the pinned prohibition
/// list: standing rules ride along with the request but are not work the user asked for.
fn is_knowledge_item(item: &str) -> bool {
    let Some(inner) = item.strip_prefix('[').and_then(|rest| rest.split_once(']')).map(|(id, _)| id)
    else {
        return false;
    };
    inner
        .rsplit_once('-')
        .is_some_and(|(kind, number)| {
            !kind.is_empty()
                && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
        })
}

/// Files named in one list item: path-shaped words (they contain `/` or `\\`, or end in a short
/// alphabetic extension such as `.rs`), without quotes, backticks or brackets around them.
fn paths_in_item(item: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in item.split(|c: char| c == ',' || c == ';' || c == '、' || c == '，' || c.is_whitespace()) {
        let path = token
            .trim_matches(|c: char| {
                matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '。' | '：' | ':' | '「' | '」')
            })
            .trim_end_matches(['.', ':']);
        if path.is_empty() || path.contains("://") {
            continue;
        }
        let has_sep = path.contains('/') || path.contains('\\');
        let has_ext = path.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && ext.chars().any(|c| c.is_ascii_alphabetic())
        });
        if (has_sep || has_ext) && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

/// The list items of the request as tasks. Each task's write set is what its own item names; an
/// item that names no file is left with an empty set, which the estimate reads as "independence
/// not established".
pub fn detect_tasks(user_text: &str) -> Vec<Task> {
    detect_tasks_with_secs(user_text, DEFAULT_TASK_SECONDS)
}

/// [`detect_tasks`] with an explicit expected duration per task.
pub fn detect_tasks_with_secs(user_text: &str, est_seconds: f64) -> Vec<Task> {
    user_text
        .lines()
        .filter_map(list_item)
        .filter(|s| !s.is_empty() && !is_knowledge_item(s))
        .map(|s| Task {
            name: s.chars().take(60).collect(),
            write_paths: paths_in_item(s),
            est_seconds,
        })
        .collect()
}

/// For a request with two or more list items: the estimate (always), and the recommendation text
/// only when the estimate says to split, i.e. every item names files and the sets are disjoint.
pub fn recommend_for_input(
    user_text: &str,
    cfg: &EstimateConfig,
) -> Option<(Estimate, Option<String>)> {
    // Messages the host injects (an advisor consultation, a sub-agent notification) are not a
    // request: their bullet lists are recent actions and results, not work to split up.
    let head = user_text.trim_start();
    if ["[advisor]", "<subagent_notification>"].iter().any(|p| head.starts_with(p)) {
        return None;
    }
    let tasks = detect_tasks_with_secs(user_text, cfg.default_task_secs);
    if tasks.len() < 2 {
        return None;
    }
    let e = estimate(&tasks, cfg);
    let text = (e.decision == crate::estimate::Decision::Multi).then(|| recommendation_text(&e));
    Some((e, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Decision;

    #[test]
    fn no_list_no_recommendation() {
        assert!(recommend_for_input("fix the bug", &EstimateConfig::default()).is_none());
    }

    #[test]
    fn a_plain_list_of_steps_is_not_recommended_for_splitting() {
        let text = "Do these:\n1. add the API\n2. write the CLI\n- update docs";
        let (e, rec) = recommend_for_input(text, &EstimateConfig::default()).unwrap();
        assert_eq!(e.decision, Decision::Single);
        assert!(e.reason.contains("independence not established"));
        assert!(rec.is_none());
        assert_eq!(e.tasks.len(), 3);
    }

    #[test]
    fn items_that_name_disjoint_files_are_recommended_for_splitting() {
        let text = "Do these:\n1. add the API in core/src/api.rs\n2. write the CLI in `cli/src/main.rs`";
        let (e, rec) = recommend_for_input(text, &EstimateConfig::default()).unwrap();
        assert_eq!(e.decision, Decision::Multi);
        let rec = rec.expect("recommendation");
        assert!(rec.contains("recommended"));
        assert!(!rec.contains("cost ~"));
        assert_eq!(e.tasks[0].write_paths, vec!["core/src/api.rs"]);
        assert_eq!(e.tasks[1].write_paths, vec!["cli/src/main.rs"]);
    }

    #[test]
    fn items_that_name_the_same_file_stay_single() {
        let text = "- change core/src/a.rs\n- also change core/src/a.rs";
        let (e, rec) = recommend_for_input(text, &EstimateConfig::default()).unwrap();
        assert_eq!(e.decision, Decision::Single);
        assert!(e.reason.contains("overlap"));
        assert!(rec.is_none());
    }

    #[test]
    fn one_item_without_files_makes_independence_unknown() {
        let text = "- change core/src/a.rs\n- update the docs";
        let (e, rec) = recommend_for_input(text, &EstimateConfig::default()).unwrap();
        assert_eq!(e.decision, Decision::Single);
        assert!(rec.is_none());
    }

    #[test]
    fn injected_advisor_and_notification_messages_are_not_requests() {
        let advisor = "[advisor] Stall detected (no_progress).
Recent actions:
- web_search {\"query\":\"a\"} -> ok
- web_search {\"query\":\"b\"} -> ok";
        assert!(recommend_for_input(advisor, &EstimateConfig::default()).is_none());
        let note = "<subagent_notification>
- a
- b
</subagent_notification>";
        assert!(recommend_for_input(note, &EstimateConfig::default()).is_none());
    }

    #[test]
    fn pinned_rules_are_not_tasks() {
        let text = "禁止事項(常に守る):
- [rule-0005] プロセスを名前で止めない (理由: x)
- [rule-0007] consume を呼ばない

Tesla V100 を調べて";
        assert!(recommend_for_input(text, &EstimateConfig::default()).is_none());
        assert!(detect_tasks(text).is_empty());
        let mixed = format!("{text}
- add the API in core/src/api.rs
- write the CLI in cli/src/main.rs");
        let (e, _) = recommend_for_input(&mixed, &EstimateConfig::default()).unwrap();
        assert_eq!(e.tasks.len(), 2);
        assert_eq!(e.tasks[0].write_paths, vec!["core/src/api.rs"]);
    }

    #[test]
    fn paths_are_words_with_a_separator_or_an_extension() {
        assert_eq!(
            paths_in_item("edit src/lib.rs, tests\\a.py (and v1.2 at https://x.y/z)"),
            vec!["src/lib.rs", "tests\\a.py"]
        );
        assert!(paths_in_item("make it faster").is_empty());
    }
}
