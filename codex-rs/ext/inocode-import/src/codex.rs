//! Codex sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (and `archived_sessions/`).
//!
//! A newer rollout records each finished item (`event_msg` / `item_completed`): the user's message,
//! the agent's, its reasoning (readable summary and raw text), commands with their output, and the
//! other tool calls. An older one has the legacy events (`user_message`, `agent_message`,
//! `agent_reasoning`) and the calls as response items. Encrypted reasoning cannot be read and is
//! left out.

use crate::Converted;
use crate::items::Builder;
use crate::parse_ts;
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

fn texts(parts: Option<&Value>) -> String {
    match parts {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.clone()),
                other => other.get("text").and_then(Value::as_str).map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn output_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("content")
            .map(|c| texts(Some(c)))
            .filter(|s| !s.is_empty())
            .or_else(|| o.get("output").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| Value::Object(o.clone()).to_string()),
        Some(Value::Array(_)) => texts(v),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// One finished item of a newer rollout.
fn item(b: &mut Builder, it: &Value, ts: Option<i64>) {
    let kind = it.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "UserMessage" | "userMessage" => b.user(&texts(it.get("content")), ts),
        "AgentMessage" | "agentMessage" => {
            let t = texts(it.get("content"));
            let t = if t.is_empty() { it.get("text").and_then(Value::as_str).unwrap_or_default().to_string() } else { t };
            b.agent(&t, ts);
        }
        "Reasoning" | "reasoning" => {
            let raw = texts(it.get("raw_content"));
            let t = if raw.trim().is_empty() { texts(it.get("summary_text")) } else { raw };
            b.reasoning(&t, ts);
        }
        "CommandExecution" | "commandExecution" => {
            let command = it.get("command").cloned().unwrap_or(Value::Null);
            let out = it.get("aggregated_output").and_then(Value::as_str).unwrap_or_default().to_string();
            let failed = it.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
            b.tool("shell", serde_json::json!({ "command": command, "cwd": it.get("cwd") }), Some((out, failed)), ts);
        }
        "McpToolCall" | "mcpToolCall" | "DynamicToolCall" | "dynamicToolCall" => {
            let name = it.get("tool").and_then(Value::as_str).unwrap_or(kind).to_string();
            let args = it.get("arguments").cloned().unwrap_or(Value::Null);
            let failed = it.get("error").is_some_and(|e| !e.is_null());
            let out = if failed {
                output_text(it.get("error"))
            } else {
                output_text(it.get("result").or_else(|| it.get("content_items")))
            };
            b.tool(&name, args, Some((out, failed)), ts);
        }
        "" | "Plan" | "ContextCompaction" | "HookPrompt" => {}
        _ => {
            let mut input = it.clone();
            if let Some(o) = input.as_object_mut() {
                o.remove("id");
                o.remove("type");
            }
            b.tool(kind, input, None, ts);
        }
    }
}

pub(crate) fn convert(path: &Path) -> std::io::Result<Converted> {
    let file = std::fs::File::open(path)?;
    let mut lines: Vec<Value> = Vec::new();
    let mut broken = 0usize;
    let mut cwd: Option<String> = None;
    let mut has_items = false;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            broken += 1;
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let sub = v.pointer("/payload/type").and_then(Value::as_str).unwrap_or_default();
        if kind == "session_meta" && cwd.is_none() {
            cwd = v.pointer("/payload/cwd").and_then(Value::as_str).map(str::to_string);
            continue;
        }
        if kind == "turn_context" && cwd.is_none() {
            cwd = v.pointer("/payload/cwd").and_then(Value::as_str).map(str::to_string);
        }
        has_items |= kind == "event_msg" && sub == "item_completed";
        if kind == "event_msg" || kind == "response_item" {
            lines.push(v);
        }
    }

    let mut b = Builder::new("Codex");
    if has_items {
        for v in &lines {
            if v.pointer("/payload/type").and_then(Value::as_str) != Some("item_completed") {
                continue;
            }
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            if let Some(it) = v.pointer("/payload/item") {
                item(&mut b, it, ts);
            }
        }
    } else {
        // Older rollouts: the legacy events for the messages, the response items for the calls.
        let outputs: HashMap<String, (String, bool)> = lines
            .iter()
            .filter_map(|v| {
                let p = v.get("payload")?;
                match p.get("type").and_then(Value::as_str)? {
                    "function_call_output" | "custom_tool_call_output" => {
                        let id = p.get("call_id")?.as_str()?.to_string();
                        let failed = p.pointer("/output/success").and_then(Value::as_bool) == Some(false);
                        Some((id, (output_text(p.get("output")), failed)))
                    }
                    _ => None,
                }
            })
            .collect();
        for v in &lines {
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(parse_ts);
            let Some(p) = v.get("payload") else { continue };
            let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
            match (kind, p.get("type").and_then(Value::as_str).unwrap_or_default()) {
                ("event_msg", "user_message") => {
                    b.user(p.get("message").and_then(Value::as_str).unwrap_or_default(), ts)
                }
                ("event_msg", "agent_message") => {
                    b.agent(p.get("message").and_then(Value::as_str).unwrap_or_default(), ts)
                }
                ("event_msg", "agent_reasoning") => {
                    b.reasoning(p.get("text").and_then(Value::as_str).unwrap_or_default(), ts)
                }
                ("response_item", "function_call" | "custom_tool_call") => {
                    let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let input = match p.get("arguments").or_else(|| p.get("input")) {
                        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                        Some(other) => other.clone(),
                        None => Value::Null,
                    };
                    let id = p.get("call_id").and_then(Value::as_str).unwrap_or_default();
                    b.tool(name, input, outputs.get(id).cloned(), ts);
                }
                _ => {}
            }
        }
    }
    let (items, user_texts, first_user, context_tokens) = b.finish();
    Ok(Converted { cwd, title: None, first_user, items, user_texts, broken_lines: broken, context_tokens })
}
