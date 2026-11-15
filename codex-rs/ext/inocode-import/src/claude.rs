//! Claude Code sessions: `~/.claude/projects/<project>/<session>.jsonl`, one JSON record a line.
//!
//! The conversation is the `user` and `assistant` records. A rewind or a regenerated answer leaves
//! branches: the main line is the one that ends at the last message, walked back through
//! `parentUuid`. Records of sub-agents (`isSidechain`) and the client's own notes (`isMeta`) are
//! left out. Thinking, tool calls and tool results are kept whole.

use crate::Converted;
use crate::items::Builder;
use crate::parse_ts;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;

enum Ev {
    User(String, Option<i64>),
    Agent(String, Option<i64>),
    Think(String, Option<i64>),
    Tool { name: String, input: Value, ts: Option<i64>, output: Option<(String, bool)> },
}

fn text_of_result(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn conversational(record: &Value) -> bool {
    matches!(record.get("type").and_then(Value::as_str), Some("user" | "assistant"))
        && record.get("isSidechain").and_then(Value::as_bool) != Some(true)
}

pub(crate) fn convert(path: &Path) -> std::io::Result<Converted> {
    let file = std::fs::File::open(path)?;
    let mut records: Vec<Value> = Vec::new();
    let mut broken = 0usize;
    let mut cwd: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            broken += 1;
            continue;
        };
        if cwd.is_none() {
            cwd = record.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => {
                custom_title = record.get("customTitle").and_then(Value::as_str).map(str::to_string);
            }
            Some("ai-title") => {
                ai_title = record.get("aiTitle").and_then(Value::as_str).map(str::to_string);
            }
            _ => {}
        }
        if conversational(&record) && record.get("isMeta").and_then(Value::as_bool) != Some(true) {
            records.push(record);
        }
    }

    // The main line: from the last message back through parentUuid.
    let index: HashMap<String, usize> = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.get("uuid").and_then(Value::as_str).map(|u| (u.to_string(), i)))
        .collect();
    let mut keep: HashSet<usize> = HashSet::new();
    let mut at = records.len().checked_sub(1);
    while let Some(i) = at {
        if !keep.insert(i) {
            break;
        }
        at = records[i]
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|p| index.get(p).copied());
    }
    // Records with no uuid (older formats) stay in file order.
    for (i, r) in records.iter().enumerate() {
        if r.get("uuid").is_none() {
            keep.insert(i);
        }
    }

    let mut evs: Vec<Ev> = Vec::new();
    let mut tool_at: HashMap<String, usize> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        if !keep.contains(&i) {
            continue;
        }
        let ts = record.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
        let role = record.get("type").and_then(Value::as_str).unwrap_or_default();
        let content = record.pointer("/message/content");
        match (role, content) {
            ("user", Some(Value::String(text))) => evs.push(Ev::User(text.clone(), ts)),
            ("user", Some(Value::Array(blocks))) => {
                let mut texts = Vec::new();
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                texts.push(t.to_string());
                            }
                        }
                        Some("image") => texts.push("[image]".to_string()),
                        Some("tool_result") => {
                            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or_default();
                            let out = text_of_result(b.get("content"));
                            let failed = b.get("is_error").and_then(Value::as_bool) == Some(true);
                            if let Some(Ev::Tool { output, .. }) = tool_at.get(id).and_then(|&k| evs.get_mut(k)) {
                                *output = Some((out, failed));
                            }
                        }
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    evs.push(Ev::User(texts.join("\n"), ts));
                }
            }
            ("assistant", Some(Value::Array(blocks))) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                evs.push(Ev::Agent(t.to_string(), ts));
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                                evs.push(Ev::Think(t.to_string(), ts));
                            }
                        }
                        Some("tool_use") => {
                            let id = b.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                            tool_at.insert(id.clone(), evs.len());
                            evs.push(Ev::Tool {
                                name: b.get("name").and_then(Value::as_str).unwrap_or("tool").to_string(),
                                input: b.get("input").cloned().unwrap_or(Value::Null),
                                ts,
                                output: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            ("assistant", Some(Value::String(text))) => evs.push(Ev::Agent(text.clone(), ts)),
            _ => {}
        }
    }

    let mut b = Builder::new("Claude Code");
    for ev in evs {
        match ev {
            Ev::User(t, ts) => b.user(&t, ts),
            Ev::Agent(t, ts) => b.agent(&t, ts),
            Ev::Think(t, ts) => b.reasoning(&t, ts),
            Ev::Tool { name, input, ts, output } => b.tool(&name, input, output, ts),
        }
    }
    let (items, user_texts, first_user, context_tokens) = b.finish();
    Ok(Converted {
        cwd,
        title: custom_title.or(ai_title),
        first_user,
        items,
        user_texts,
        broken_lines: broken,
        context_tokens,
    })
}
