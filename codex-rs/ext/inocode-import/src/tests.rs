use super::*;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;

fn write_jsonl(dir: &Path, name: &str, lines: &[serde_json::Value]) -> PathBuf {
    let p = dir.join(name);
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    std::fs::write(&p, text.join("\n") + "\nnot json\n").unwrap();
    p
}

/// What the screen shows, in order: "user:…", "agent:…", "think:…", "tool:<name>:<output>".
fn shown(items: &[RolloutItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            RolloutItem::EventMsg(EventMsg::UserMessage(u)) => Some(format!("user:{}", u.message)),
            RolloutItem::EventMsg(EventMsg::AgentMessage(a)) => Some(format!("agent:{}", a.message)),
            RolloutItem::EventMsg(EventMsg::AgentReasoning(r)) => Some(format!("think:{}", r.text)),
            RolloutItem::EventMsg(EventMsg::McpToolCallEnd(e)) => {
                let out = match &e.result {
                    Ok(r) => r.content.first().and_then(|c| c.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string(),
                    Err(m) => format!("ERR {m}"),
                };
                Some(format!("tool:{}:{}", e.invocation.tool, out))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_claude_session_keeps_thinking_tools_and_the_main_line_only() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_jsonl(
        dir.path(),
        "s.jsonl",
        &[
            json!({"type":"user","uuid":"u1","parentUuid":null,"cwd":"D:/p","timestamp":"2026-11-01T00:00:00Z","message":{"role":"user","content":"消して out/ を"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"content":[
                {"type":"thinking","thinking":"考える"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls out"}}]}}),
            json!({"type":"user","uuid":"r1","parentUuid":"a1","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"a.txt\nb.txt"}]}}),
            // a branch that was rewound: not on the main line
            json!({"type":"assistant","uuid":"old","parentUuid":"r1","message":{"content":[{"type":"text","text":"古い答え"}]}}),
            json!({"type":"assistant","uuid":"side","parentUuid":"r1","isSidechain":true,"message":{"content":[{"type":"text","text":"サブ"}]}}),
            json!({"type":"user","uuid":"meta","parentUuid":"r1","isMeta":true,"message":{"content":"caveat"}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"r1","message":{"content":[{"type":"text","text":"新しい答え"}]}}),
            json!({"type":"ai-title","aiTitle":"出力の整理"}),
        ],
    );
    let c = convert(&p, Source::Claude).unwrap();
    assert_eq!(
        shown(&c.items),
        vec!["user:消して out/ を", "think:考える", "tool:Bash:a.txt\nb.txt", "agent:新しい答え"]
    );
    assert_eq!(c.user_texts, vec!["消して out/ を"]);
    assert_eq!(c.cwd.as_deref(), Some("D:/p"));
    assert_eq!(c.title.as_deref(), Some("出力の整理"));
    assert_eq!(c.broken_lines, 1);
}

#[test]
fn a_newer_codex_rollout_is_read_from_its_finished_items() {
    let dir = tempfile::tempdir().unwrap();
    let item = |it: serde_json::Value| json!({"timestamp":"2026-11-09T05:17:43.000Z","type":"event_msg","payload":{"type":"item_completed","item":it}});
    let p = write_jsonl(
        dir.path(),
        "rollout.jsonl",
        &[
            json!({"type":"session_meta","payload":{"cwd":"D:/q","id":"x"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}),
            item(json!({"type":"UserMessage","id":"1","content":[{"type":"text","text":"テストして"}]})),
            item(json!({"type":"Reasoning","id":"2","summary_text":["要約"],"raw_content":[]})),
            item(json!({"type":"CommandExecution","id":"3","command":"cargo test","aggregated_output":"FAILED","exit_code":101})),
            item(json!({"type":"AgentMessage","id":"4","content":[{"type":"Text","text":"失敗しました"}]})),
        ],
    );
    let c = convert(&p, Source::Codex).unwrap();
    assert_eq!(shown(&c.items), vec!["user:テストして", "think:要約", "tool:shell:ERR FAILED", "agent:失敗しました"]);
    assert_eq!(c.cwd.as_deref(), Some("D:/q"));
}

#[test]
fn an_older_codex_rollout_is_read_from_its_legacy_events_and_calls() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_jsonl(
        dir.path(),
        "rollout.jsonl",
        &[
            json!({"type":"event_msg","payload":{"type":"user_message","message":"見て"}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"c1"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"x.rs"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"x.rs があります"}}),
        ],
    );
    let c = convert(&p, Source::Codex).unwrap();
    assert_eq!(shown(&c.items), vec!["user:見て", "tool:shell:x.rs", "agent:x.rs があります"]);
}

#[test]
fn times_with_and_without_an_offset() {
    assert_eq!(parse_ts("1970-01-01T00:00:10Z"), Some(10));
    assert_eq!(parse_ts("2026-11-14T13:45:21.552Z"), Some(1_794_663_921));
    assert_eq!(parse_ts("2026-11-14T22:45:21+09:00"), Some(1_794_663_921));
    assert_eq!(parse_ts("yesterday"), None);
}

#[test]
fn it_asks_once_per_thread_and_never_in_a_quiet_project() {
    let mut r = Record::default();
    r.threads.insert("t".into(), ImportedThread { source: Some(Source::Claude), path: "p".into(), cwd: String::new(), extraction: Extraction::None, context_tokens: 0 });
    assert!(should_ask(&r, "t", Some("D:\\work\\a")));
    assert!(!should_ask(&r, "other", None));
    r.threads.get_mut("t").unwrap().cwd = "D:/work/a".into();
    r.quiet_projects.push("D:/work/a".into());
    assert!(!should_ask(&r, "t", None));
    r.quiet_projects.clear();
    r.threads.get_mut("t").unwrap().cwd = String::new();
    r.quiet_projects.push("d:/work/a/".into());
    assert!(!should_ask(&r, "t", Some("D:\\work\\a")));
    assert!(should_ask(&r, "t", Some("D:\\work\\b")));
    r.threads.get_mut("t").unwrap().extraction = Extraction::Started;
    assert!(!should_ask(&r, "t", Some("D:\\work\\b")));
}

#[test]
fn detect_lists_sessions_and_memories_and_what_was_imported() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join(".claude");
    let project = claude.join("projects").join("D--p");
    std::fs::create_dir_all(project.join("memory")).unwrap();
    let s = write_jsonl(&project, "a.jsonl", &[json!({"type":"user","cwd":"D:/p","message":{"content":"hi"}})]);
    std::fs::write(project.join("memory").join("MEMORY.md"), "- 覚え").unwrap();
    std::fs::write(claude.join("CLAUDE.md"), "global").unwrap();
    let home = dir.path().join("ino");
    let mut r = Record::default();
    let size = std::fs::metadata(&s).unwrap().len();
    r.files.insert(
        s.to_string_lossy().to_string(),
        ImportedFile { source: Some(Source::Claude), sha256: "x".into(), size_bytes: size, thread_id: "T".into(), imported_at: 0 },
    );
    write_record(&home, &r).unwrap();
    let roots = Roots { claude: Some(claude.clone()), codex: None };
    let found = detect(&roots, &home);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].cwd.as_deref(), Some("D:/p"));
    assert_eq!(found[0].thread_id.as_deref(), Some("T"));
    let mems = detect_memories(&roots);
    let names: Vec<(String, Option<String>)> = mems
        .iter()
        .map(|m| (Path::new(&m.path).file_name().unwrap().to_string_lossy().to_string(), m.cwd.clone()))
        .collect();
    assert_eq!(names, vec![("CLAUDE.md".to_string(), None), ("MEMORY.md".to_string(), Some("D:/p".to_string()))]);
}
