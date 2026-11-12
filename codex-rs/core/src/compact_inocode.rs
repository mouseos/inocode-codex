//! InoCode additions to local compaction: re-attaching recently accessed files,
//! a per-thread consecutive-failure circuit breaker, and pinned-rule re-injection.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;

/// Team estimate settings from `team.*` parameters, read at the point of use.
pub(crate) fn estimate_config() -> codex_inocode_team::EstimateConfig {
    use codex_inocode_flags::float_param;
    codex_inocode_team::EstimateConfig {
        threshold: float_param("team.speedup_threshold"),
        split_overhead_secs: float_param("team.split_overhead_secs"),
        integrate_overhead_secs: float_param("team.integrate_overhead_secs"),
        default_task_secs: float_param("team.task_secs"),
        ..Default::default()
    }
}

/// Files re-attached after a local compaction (`compaction.reattach_files`).
pub(crate) fn max_reattached_files() -> usize {
    usize::try_from(codex_inocode_flags::int_param("compaction.reattach_files")).unwrap_or(usize::MAX)
}
/// Total token budget for all re-attached files.
pub(crate) fn max_reattached_tokens() -> usize {
    usize::try_from(codex_inocode_flags::int_param("compaction.reattach_tokens")).unwrap_or(usize::MAX)
}
/// Consecutive auto-compaction failures after which the session stops auto-compacting.
pub(crate) fn max_consecutive_compact_failures() -> u32 {
    codex_inocode_compaction::max_consecutive_failures()
}

/// Working directory of the turn, as a local path.
#[allow(deprecated)]
pub(crate) fn local_cwd(turn: &crate::session::turn_context::TurnContext) -> PathBuf {
    turn.cwd.as_path().to_path_buf()
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

fn failures() -> &'static Mutex<HashMap<String, u32>> {
    static MAP: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True once `thread_id` has failed auto-compaction too many times in a row.
pub(crate) fn auto_compact_tripped(thread_id: &str) -> bool {
    let map = failures().lock().unwrap_or_else(|e| e.into_inner());
    map.get(thread_id)
        .is_some_and(|n| *n >= max_consecutive_compact_failures())
}

pub(crate) fn record_compact_result(thread_id: &str, ok: bool) {
    let mut map = failures().lock().unwrap_or_else(|e| e.into_inner());
    if ok {
        map.remove(thread_id);
    } else {
        *map.entry(thread_id.to_string()).or_insert(0) += 1;
    }
}

fn failure_count(thread_id: &str) -> u32 {
    failures()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(thread_id)
        .copied()
        .unwrap_or(0)
}

/// Reports one finished compaction (`inocode/compaction/event`). Call after
/// [`record_compact_result`] for automatic runs. A failure that reaches the limit is followed
/// by a `breakerTripped` event.
pub(crate) fn emit_compaction_result<E: std::fmt::Debug>(
    thread_id: &str,
    auto: bool,
    reason: &str,
    result: &Result<(), E>,
) {
    use codex_inocode_compaction::CompactionEventKind;
    let failures = failure_count(thread_id);
    match result {
        Ok(()) => {
            codex_inocode_compaction::emit(thread_id, CompactionEventKind::Completed, auto, reason, failures);
        }
        Err(err) => {
            tracing::warn!("compaction failed ({reason}): {err:?}");
            codex_inocode_compaction::emit(thread_id, CompactionEventKind::Failed, auto, reason, failures);
            if auto && failures >= max_consecutive_compact_failures() {
                codex_inocode_compaction::emit(
                    thread_id,
                    CompactionEventKind::BreakerTripped,
                    auto,
                    reason,
                    failures,
                );
            }
        }
    }
}

/// Reports that an automatic compaction was refused because the breaker is open.
pub(crate) fn emit_breaker_open(thread_id: &str, reason: &str) {
    codex_inocode_compaction::emit(
        thread_id,
        codex_inocode_compaction::CompactionEventKind::BreakerTripped,
        true,
        reason,
        failure_count(thread_id),
    );
}

/// Ids of the pinned rules that would be re-injected for `cwd`.
pub(crate) fn pinned_rule_ids(cwd: &Path) -> Vec<String> {
    codex_inocode_compaction::rule_ids_in_text(&codex_inocode_knowledge::pinned_rules_text(cwd))
}

// ---------------------------------------------------------------------------
// Recent files
// ---------------------------------------------------------------------------

const READ_COMMANDS: &[&str] = &[
    "cat",
    "type",
    "get-content",
    "gc",
    "head",
    "tail",
    "bat",
    "less",
    "more",
    "nl",
];

fn clean_token(tok: &str) -> &str {
    tok.trim_matches(|c| c == '"' || c == '\'' || c == ',' || c == '[' || c == ']')
}

/// Paths named by the arguments of one tool call (reads and writes), in order.
fn paths_from_call(item: &ResponseItem) -> Vec<String> {
    let mut out = Vec::new();
    match item {
        ResponseItem::CustomToolCall { name, input, .. } if name == "apply_patch" => {
            for line in input.lines() {
                for prefix in ["*** Add File: ", "*** Update File: "] {
                    if let Some(p) = line.strip_prefix(prefix) {
                        out.push(p.trim().to_string());
                    }
                }
            }
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => {
            if name == "apply_patch" {
                for line in arguments.lines() {
                    for prefix in ["*** Add File: ", "*** Update File: "] {
                        if let Some(idx) = line.find(prefix) {
                            let rest = &line[idx + prefix.len()..];
                            out.push(rest.trim().to_string());
                        }
                    }
                }
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
                return out;
            };
            let command = value
                .get("cmd")
                .or_else(|| value.get("command"))
                .map(|c| match c {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(a) => a
                        .iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::new(),
                })
                .unwrap_or_default();
            let mut reading = false;
            for tok in command.split_whitespace() {
                if matches!(tok, "|" | ";" | "&&" | "||") {
                    reading = false;
                    continue;
                }
                let tok = clean_token(tok);
                if READ_COMMANDS.contains(&tok.to_ascii_lowercase().as_str()) {
                    reading = true;
                    continue;
                }
                if reading && !tok.starts_with('-') && !tok.is_empty() {
                    out.push(tok.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

/// Most recently touched file paths, newest first, deduplicated.
pub(crate) fn recent_file_paths<'a>(
    items: impl DoubleEndedIterator<Item = &'a ResponseItem>,
    cwd: &Path,
    limit: usize,
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items.rev() {
        let mut paths = paths_from_call(item);
        paths.reverse();
        for p in paths {
            let path = PathBuf::from(&p);
            let path = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            if seen.insert(path.clone()) {
                out.push(path);
                if out.len() >= limit * 3 {
                    // Over-collect: some of these may no longer exist.
                    return out;
                }
            }
        }
    }
    out
}

/// Reads up to `compaction.reattach_files` of the candidate files within the token budget and renders
/// them as a block appended to the compaction summary. Empty if nothing could be read.
#[cfg(test)]
pub(crate) fn render_reattached_files(candidates: &[PathBuf]) -> String {
    render_reattached_files_with_paths(candidates).0
}

/// Like [`render_reattached_files`], also returning the files that were actually attached.
pub(crate) fn render_reattached_files_with_paths(candidates: &[PathBuf]) -> (String, Vec<String>) {
    let mut remaining = max_reattached_tokens();
    let mut blocks = Vec::new();
    let mut attached = Vec::new();
    for path in candidates {
        if blocks.len() >= max_reattached_files() || remaining == 0 {
            break;
        }
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 4_000_000 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let tokens = approx_token_count(&text);
        let body = if tokens > remaining {
            truncate_text(&text, TruncationPolicy::Tokens(remaining))
        } else {
            text
        };
        remaining = remaining.saturating_sub(approx_token_count(&body));
        blocks.push(format!("--- {} ---\n{body}", path.display()));
        attached.push(path.display().to_string());
    }
    if blocks.is_empty() {
        return (String::new(), Vec::new());
    }
    (
        format!(
            "Recently accessed files (re-attached after compaction; current content):\n\n{}",
            blocks.join("\n\n")
        ),
        attached,
    )
}

/// Pinned prohibition rules, rendered for re-injection after compaction or a model switch.
pub(crate) fn pinned_rules_block(cwd: &Path) -> String {
    let text = codex_inocode_knowledge::pinned_rules_text(cwd);
    if text.trim().is_empty() {
        return String::new();
    }
    format!("Pinned rules (always apply):\n{}", text.trim_end())
}

/// Pinned rules as a user message to place in the initial context, if any apply.
pub(crate) fn pinned_rules_item(cwd: &Path) -> Option<ResponseItem> {
    let text = pinned_rules_block(cwd);
    if text.is_empty() {
        return None;
    }
    Some(ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_owned(),
        content: vec![codex_protocol::models::ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

// ---------------------------------------------------------------------------
// Instruction ledger (the user's own messages, verbatim)
// ---------------------------------------------------------------------------

fn metric(thread_id: &str, event: codex_inocode_metrics::MetricEvent) {
    if codex_inocode_flags::InocodeFlags::load().metrics_enabled {
        codex_inocode_metrics::record_global(thread_id, event);
    }
}

/// Stores the user's message in the ledger, verbatim. Sub-agents are not users: their input
/// comes from the parent agent, so it is not recorded.
pub(crate) fn ledger_record(
    thread_id: &str,
    session_source: &codex_protocol::protocol::SessionSource,
    user_input: &[codex_protocol::user_input::UserInput],
) {
    if matches!(session_source, codex_protocol::protocol::SessionSource::SubAgent(_)) {
        return;
    }
    let text: String = user_input
        .iter()
        .filter_map(|u| match u {
            codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if codex_inocode_ledger::record(thread_id, &text).is_some() {
        metric(thread_id, codex_inocode_metrics::MetricEvent::LedgerRecorded);
    }
}

/// Plain text of the user messages a compacted history still holds.
pub(crate) fn retained_user_texts(items: &[codex_history::ResponseItemEnvelope]) -> Vec<String> {
    items
        .iter()
        .filter_map(|envelope| match &envelope.item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                crate::compact::content_items_to_text(content)
            }
            _ => None,
        })
        .collect()
}

/// The ledger, presented again after a compaction at the same fixed place as the pinned rules.
/// Messages the compacted history still holds are not repeated; a long message is presented as its
/// beginning plus the path of the whole text (`compaction.ledger_preview_chars`).
pub(crate) fn ledger_preview_chars() -> usize {
    usize::try_from(codex_inocode_flags::int_param("compaction.ledger_preview_chars"))
        .unwrap_or(usize::MAX)
}

pub(crate) fn ledger_item(
    thread_id: &str,
    retained: &[codex_history::ResponseItemEnvelope],
) -> Option<ResponseItem> {
    let rendered = codex_inocode_ledger::render(
        thread_id,
        &retained_user_texts(retained),
        ledger_preview_chars(),
    )?;
    metric(
        thread_id,
        codex_inocode_metrics::MetricEvent::LedgerRepresented {
            full: rendered.full as u64,
            marked: rendered.marked as u64,
        },
    );
    Some(user_text_item(rendered.text))
}

// ---------------------------------------------------------------------------
// Old observations -> files, before a compaction
// ---------------------------------------------------------------------------

/// Offloads the text of tool outputs older than the newest `compaction.keep_recent_observations`
/// to `tool-results/` files (lossless: the whole output stays in the file), leaving the same
/// preview-and-path stub the large-output persistence uses. Returns (outputs, bytes) offloaded.
pub(crate) fn offload_old_observations(thread_id: &str, items: &mut [ResponseItem]) -> (u64, u64) {
    if !codex_inocode_flags::InocodeFlags::load().compaction_persist_large_output {
        return (0, 0);
    }
    let keep = usize::try_from(codex_inocode_flags::int_param("compaction.keep_recent_observations"))
        .unwrap_or(usize::MAX);
    let preview = usize::try_from(codex_inocode_flags::int_param("compaction.preview_bytes"))
        .unwrap_or(usize::MAX);
    let mut config = codex_inocode_tool_output::PersistConfig::new(
        true,
        /*threshold_tokens*/ 0,
        &codex_inocode_flags::inocode_home(),
    );
    config.preview_bytes = preview;
    let (count, bytes) = offload_old_observations_with(thread_id, items, keep, &config);
    if count > 0 {
        metric(
            thread_id,
            codex_inocode_metrics::MetricEvent::ObservationsOffloaded { count, bytes },
        );
    }
    (count, bytes)
}

fn offload_old_observations_with(
    thread_id: &str,
    items: &mut [ResponseItem],
    keep: usize,
    config: &codex_inocode_tool_output::PersistConfig,
) -> (u64, u64) {
    let outputs: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            matches!(
                item,
                ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
            )
        })
        .map(|(i, _)| i)
        .collect();
    if outputs.len() <= keep {
        return (0, 0);
    }
    let (mut count, mut bytes) = (0u64, 0u64);
    for &i in &outputs[..outputs.len() - keep] {
        let (call_id, output) = match &mut items[i] {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => (call_id.clone().unwrap_or_default(), output),
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => (call_id.clone(), output),
            _ => continue,
        };
        // Shell and code-mode results come as content items, not a plain string: handle both.
        let meta = codex_inocode_tool_output::PersistMeta {
            tool: None,
            thread_id: Some(thread_id.to_string()),
            call_id: (!call_id.is_empty()).then_some(call_id),
        };
        for text in output_texts(&mut output.body) {
            // A stub bigger than the output saves nothing.
            if text.len() <= config.preview_bytes.saturating_mul(2) {
                continue;
            }
            let original = text.len() as u64;
            if let Some(stub) =
                codex_inocode_tool_output::persist_if_large_with(text, config, &meta)
            {
                *text = stub;
                count += 1;
                bytes += original;
            }
        }
    }
    (count, bytes)
}

/// A compaction drops the old tool outputs together with their stubs, so the model would no longer
/// know where the full text went. This lists the files this thread saved (path, tool, size) so it
/// can read one back with the shell when it needs the text.
fn saved_outputs_text(dir: &std::path::Path, thread_id: &str) -> Option<String> {
    let mut lines = Vec::new();
    for entry in codex_inocode_tool_output::list_outputs(dir) {
        if entry.thread_id.as_deref() != Some(thread_id) {
            continue;
        }
        let path = dir.join(format!("{}.txt", entry.id));
        let what = entry
            .tool
            .or(entry.call_id)
            .unwrap_or_else(|| "tool output".to_string());
        lines.push(format!("- {} ({what}, {} bytes)", path.display(), entry.bytes));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "Full text of earlier tool outputs, saved to files (read one with the shell if you need it):\n{}",
        lines.join("\n")
    ))
}

pub(crate) fn saved_outputs_item(thread_id: &str) -> Option<ResponseItem> {
    let dir = codex_inocode_flags::inocode_home().join("tool-results");
    saved_outputs_text(&dir, thread_id).map(user_text_item)
}

/// The text parts of a tool output: the string itself, or each text content item.
pub(crate) fn output_texts(
    body: &mut codex_protocol::models::FunctionCallOutputBody,
) -> Vec<&mut String> {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;
    match body {
        FunctionCallOutputBody::Text(text) => vec![text],
        FunctionCallOutputBody::ContentItems(items) => items
            .iter_mut()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text),
                _ => None,
            })
            .collect(),
    }
}

/// Items to record at the start of a turn: the `team.auto_select` recommendation for a request
/// that holds several independent tasks, and the pinned rules again after a model switch.
/// Must run before `previous_turn_settings` is overwritten for this turn.
pub(crate) async fn turn_extras(
    sess: &crate::session::session::Session,
    turn: &crate::session::turn_context::TurnContext,
    user_input: &[codex_protocol::user_input::UserInput],
    typed_input: &[codex_protocol::user_input::UserInput],
) -> Vec<ResponseItem> {
    let mut items = Vec::new();
    ledger_record(&sess.thread_id.to_string(), &turn.session_source, typed_input);
    let flags = codex_inocode_flags::InocodeFlags::load();
    if flags.team_auto_select {
        let text: String = user_input
            .iter()
            .filter_map(|u| match u {
                codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("
");
        if let Some((estimate, rec)) = codex_inocode_team::recommend_for_input(
            &text,
            &estimate_config(),
        ) {
            codex_inocode_team::roster_for(&sess.thread_id.to_string()).set_estimate(estimate);
            if let Some(rec) = rec {
                items.push(user_text_item(rec));
            }
        }
    }
    let previous = sess.previous_turn_settings().await;
    // The very first turn of a thread has no previous settings: give the model the pinned
    // prohibition list right away, not only after a compaction or a model switch.
    if previous.is_none() {
        if let Some(item) = pinned_rules_item(&local_cwd(turn)) {
            items.push(item);
        }
    }
    if let Some(previous) = previous
        && previous.model != turn.model_info().slug
    {
        let cwd = local_cwd(turn);
        if let Some(item) = pinned_rules_item(&cwd) {
            items.push(item);
        }
        let task: String = user_input
            .iter()
            .filter_map(|u| match u {
                codex_protocol::user_input::UserInput::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let pack = codex_inocode_knowledge::build_handoff_pack(&cwd, &task);
        if !pack.trim().is_empty() {
            items.push(user_text_item(pack));
        }
    }
    items
}

fn user_text_item(text: String) -> ResponseItem {
    ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "user".to_owned(),
        content: vec![codex_protocol::models::ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_trips_after_three_and_resets() {
        let id = "t-breaker";
        assert!(!auto_compact_tripped(id));
        for _ in 0..2 {
            record_compact_result(id, false);
        }
        assert!(!auto_compact_tripped(id));
        record_compact_result(id, false);
        assert!(auto_compact_tripped(id));
        record_compact_result(id, true);
        assert!(!auto_compact_tripped(id));
    }

    fn custom(name: &str, input: &str) -> ResponseItem {
        ResponseItem::CustomToolCall {
            status: None,
            encrypted_content: None,
            id: None,
            call_id: "c".into(),
            name: name.into(),
            namespace: None,
            input: input.into(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn collects_patch_paths_newest_first() {
        let items = vec![
            custom(
                "apply_patch",
                "*** Begin Patch\n*** Update File: a.rs\n*** End Patch",
            ),
            custom("apply_patch", "*** Add File: b.rs\n*** Update File: a.rs"),
        ];
        let got = recent_file_paths(items.iter(), Path::new("/w"), 5);
        assert_eq!(
            got,
            vec![PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")]
        );
    }

    #[test]
    fn reattach_respects_file_and_token_limits() {
        let d = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..8 {
            let p = d.path().join(format!("f{i}.txt"));
            std::fs::write(&p, format!("content {i}")).unwrap();
            paths.push(p);
        }
        paths.push(d.path().join("missing.txt"));
        let text = render_reattached_files(&paths);
        assert!(text.contains("content 4") && !text.contains("content 5"));
        let big = d.path().join("big.txt");
        std::fs::write(&big, "word ".repeat(400_000)).unwrap();
        let text = render_reattached_files(&[big]);
        assert!(approx_token_count(&text) <= max_reattached_tokens() + 200);
    }

    #[test]
    fn old_observations_are_offloaded_and_recent_ones_kept() {
        use codex_protocol::models::FunctionCallOutputBody;
        use codex_protocol::models::FunctionCallOutputPayload;
        let dir = tempfile::tempdir().unwrap();
        let mut config = codex_inocode_tool_output::PersistConfig::new(true, 0, dir.path());
        config.preview_bytes = 100;
        let big = "x".repeat(1000);
        let out = |i: usize, text: &str| ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(format!("c{i}")),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(text.to_string()),
                success: None,
            },
            internal_chat_message_metadata_passthrough: None,
        };
        let mut items = vec![out(0, &big), out(1, "small"), out(2, &big), out(3, &big)];
        let (n, bytes) = offload_old_observations_with("t", &mut items, 2, &config);
        assert_eq!((n, bytes), (1, 1000)); // only #0: #1 is small, #2/#3 are recent
        let text = |i: usize| match &items[i] {
            ResponseItem::FunctionCallOutput { output, .. } => match &output.body {
                FunctionCallOutputBody::Text(t) => t.clone(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        assert!(text(0).starts_with("<persisted-output>"));
        assert_eq!(text(1), "small");
        assert_eq!(text(2), big);
        // Already offloaded outputs are not processed again.
        assert_eq!(offload_old_observations_with("t", &mut items, 2, &config), (0, 0));
    }

    #[test]
    fn content_item_outputs_are_offloaded_too() {
        use codex_protocol::models::FunctionCallOutputBody;
        use codex_protocol::models::FunctionCallOutputContentItem;
        use codex_protocol::models::FunctionCallOutputPayload;
        let dir = tempfile::tempdir().unwrap();
        let mut config = codex_inocode_tool_output::PersistConfig::new(true, 0, dir.path());
        config.preview_bytes = 100;
        let big = "y".repeat(1000);
        let out = |i: usize| ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: format!("c{i}"),
            name: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText { text: big.clone() },
                ]),
                success: None,
            },
            internal_chat_message_metadata_passthrough: None,
        };
        let mut items = vec![out(0), out(1), out(2)];
        let (n, bytes) = offload_old_observations_with("t", &mut items, 1, &config);
        assert_eq!((n, bytes), (2, 2000));
        let ResponseItem::CustomToolCallOutput { output, .. } = &mut items[0] else {
            unreachable!()
        };
        assert!(output_texts(&mut output.body)[0].starts_with("<persisted-output>"));
    }

    #[test]
    fn saved_outputs_are_listed_for_the_thread_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = codex_inocode_tool_output::PersistConfig::new(true, 0, dir.path());
        config.preview_bytes = 10;
        // PersistConfig::new puts files in <home>/tool-results.
        let results = dir.path().join("tool-results");
        let meta = |t: &str| codex_inocode_tool_output::PersistMeta {
            tool: Some("exec_command".into()),
            thread_id: Some(t.into()),
            call_id: Some("c1".into()),
        };
        let text = "z".repeat(500);
        codex_inocode_tool_output::persist_if_large_with(&text, &config, &meta("mine")).unwrap();
        codex_inocode_tool_output::persist_if_large_with(&text, &config, &meta("other")).unwrap();
        let listed = saved_outputs_text(&results, "mine").unwrap();
        assert_eq!(listed.matches("exec_command, 500 bytes").count(), 1);
        assert!(saved_outputs_text(&results, "nobody").is_none());
    }

    /// The ledger holds what the user typed and nothing else. Only `TurnInput::UserInput` whose
    /// origin is the user reaches it (`typed_user_input`); a Stop-hook continuation, a goal
    /// continuation, a notification and a message from another agent arrive as response items or
    /// inter-agent messages, a scheduled heartbeat is marked, and a sub-agent's own input is skipped
    /// at the door.
    #[test]
    fn only_what_the_user_typed_reaches_the_ledger() {
        use crate::session::TurnInput;
        use codex_history::ResponseItemEnvelope;
        use codex_history::UserInputOrigin;
        use codex_protocol::models::ContentItem;
        use codex_protocol::user_input::UserInput;
        let text = |t: &str| vec![UserInput::Text { text: t.into(), text_elements: Vec::new() }];
        let user = |origin: UserInputOrigin, t: &str| TurnInput::UserInput {
            content: text(t),
            client_id: None,
            metadata: crate::session::UserInputMetadata { origin, ..Default::default() },
        };
        let synthetic = |role: &str, t: &str| {
            TurnInput::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: role.to_owned(),
                content: vec![ContentItem::InputText { text: t.to_owned() }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }))
        };
        // Stop-hook continuation prompt, goal continuation (user or developer role), a
        // notification, a scheduled heartbeat: none of them is typed.
        let not_typed = vec![
            synthetic("user", "Stop hook: keep going"),
            synthetic("developer", "Continue working toward the active thread goal."),
            synthetic("user", "<notification>build finished</notification>"),
            user(UserInputOrigin::Heartbeat, "scheduled check-in"),
        ];
        assert!(crate::session::turn::typed_user_input(&not_typed).is_empty());
        // Only the typed input comes through, with its text.
        let mixed: Vec<TurnInput> = not_typed
            .into_iter()
            .chain([user(UserInputOrigin::User, "止めないで")])
            .collect();
        let typed = crate::session::turn::typed_user_input(&mixed);
        assert_eq!(typed, text("止めないで"));
        // A sub-agent (its start text comes from the parent agent) never records, whatever it is given.
        let id = "t-ledger-synthetic-subagent";
        for source in [
            codex_protocol::protocol::SubAgentSource::Review,
            codex_protocol::protocol::SubAgentSource::Compact,
        ] {
            ledger_record(id, &codex_protocol::protocol::SessionSource::SubAgent(source), &typed);
        }
        assert!(codex_inocode_ledger::entries_at(&codex_inocode_ledger::inocode_home(), id).is_empty());
        // Empty input records nothing either (an internal continuation has no user input).
        ledger_record(id, &codex_protocol::protocol::SessionSource::Cli, &[]);
        assert!(codex_inocode_ledger::entries_at(&codex_inocode_ledger::inocode_home(), id).is_empty());
    }
}
