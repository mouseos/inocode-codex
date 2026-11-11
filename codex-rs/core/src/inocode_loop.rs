//! InoCode loop instrumentation: measurement hooks and stall detection glue.
//!
//! Every hook is a no-op when its flag in `[inocode]` of `config.toml` is off.

use codex_inocode_flags::InocodeFlags;
use codex_inocode_metrics::MetricEvent;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnCompleteEvent;

pub(crate) fn advisor_enabled() -> bool {
    InocodeFlags::load().advisor_enabled
}

pub(crate) fn metrics_enabled() -> bool {
    InocodeFlags::load().metrics_enabled
}

fn record(thread_id: ThreadId, event: MetricEvent) {
    if metrics_enabled() {
        codex_inocode_metrics::record_global(&thread_id.to_string(), event);
    }
}

pub(crate) fn record_turn_complete(thread_id: ThreadId, completed: &TurnCompleteEvent) {
    if let Some(ms) = completed.duration_ms.and_then(|ms| u64::try_from(ms).ok()) {
        record(thread_id, MetricEvent::TurnWallClock { ms });
    }
}

pub(crate) fn record_tokens(thread_id: ThreadId, usage: Option<&TokenUsage>) {
    let Some(usage) = usage else { return };
    record(
        thread_id,
        MetricEvent::Tokens {
            input: u64::try_from(usage.input_tokens).unwrap_or(0),
            output: u64::try_from(usage.output_tokens).unwrap_or(0),
        },
    );
}

/// One statistics record per completed model request: token kinds, generation speed and the
/// rate limits that came with the response.
pub(crate) fn record_request(
    thread_id: ThreadId,
    model: &str,
    provider: &str,
    account: Option<String>,
    usage: Option<&TokenUsage>,
    generation: std::time::Duration,
    rate_limits: Option<&codex_protocol::protocol::RateLimitSnapshot>,
) {
    if !metrics_enabled() {
        return;
    }
    if let Some(record) = build_request_record(
        codex_inocode_stats::now_ms(),
        &thread_id.to_string(),
        model,
        provider,
        account,
        usage,
        generation,
        rate_limits,
    ) {
        codex_inocode_stats::record_global(&record);
    }
}

/// The statistics record of one completed request. The 5-hour and weekly readings are told apart
/// by window length, and the snapshot's `limit_id` is kept so that families (`codex`, `claude`)
/// are never compared with each other.
#[allow(clippy::too_many_arguments)]
fn build_request_record(
    ts_ms: u64,
    thread_id: &str,
    model: &str,
    provider: &str,
    account: Option<String>,
    usage: Option<&TokenUsage>,
    generation: std::time::Duration,
    rate_limits: Option<&codex_protocol::protocol::RateLimitSnapshot>,
) -> Option<codex_inocode_stats::RequestRecord> {
    let usage = usage?;
    let n = |v: i64| u64::try_from(v).unwrap_or(0);
    let input = n(usage.input_tokens);
    let cached = n(usage.cached_input_tokens).min(input);
    let output = n(usage.output_tokens);
    let gen_ms = u64::try_from(generation.as_millis()).ok();
    let (mut five_hour, mut weekly) = (None, None);
    if let Some(limits) = rate_limits {
        for window in [limits.primary.as_ref(), limits.secondary.as_ref()]
            .into_iter()
            .flatten()
        {
            codex_inocode_stats::classify_window(
                window.window_minutes,
                codex_inocode_stats::LimitSample {
                    used_percent: window.used_percent,
                    resets_at: window.resets_at,
                },
                &mut five_hour,
                &mut weekly,
            );
        }
    }
    Some(codex_inocode_stats::RequestRecord {
        ts_ms,
        source: "live".into(),
        thread_id: thread_id.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
        account,
        limit_id: rate_limits.and_then(|l| l.limit_id.clone()),
        uncached_input: input - cached,
        cached_input: cached,
        output,
        reasoning: n(usage.reasoning_output_tokens),
        gen_ms,
        tok_per_s: codex_inocode_stats::RequestRecord::speed(output, gen_ms),
        five_hour,
        weekly,
    })
}

pub(crate) fn record_wait_agent_call(thread_id: ThreadId) {
    record(thread_id, MetricEvent::WaitAgentCall);
}

pub(crate) fn record_clarification(thread_id: ThreadId) {
    record(thread_id, MetricEvent::Clarification);
}

/// Who a logged agent message answers and which threads it travelled between.
#[derive(Default)]
pub(crate) struct AgentMessageMeta {
    pub(crate) reply_to: Option<String>,
    pub(crate) reply_thread: Option<String>,
    pub(crate) from_thread: Option<String>,
    pub(crate) to_thread: Option<String>,
}

/// Stores an agent-to-agent message verbatim under `INOCODE_HOME/messages/<thread>.jsonl`.
pub(crate) fn log_agent_message(
    thread_id: ThreadId,
    from: &str,
    to: &str,
    delivery: &str,
    text: &str,
) {
    log_agent_message_with(thread_id, from, to, delivery, text, AgentMessageMeta::default());
}

pub(crate) fn log_agent_message_with(
    thread_id: ThreadId,
    from: &str,
    to: &str,
    delivery: &str,
    text: &str,
    meta: AgentMessageMeta,
) {
    let flags = InocodeFlags::load();
    if !flags.protocol_enabled && !flags.peer_messaging_enabled {
        return;
    }
    if text.trim_start().starts_with("ASK") && flags.metrics_enabled {
        record_clarification(thread_id);
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Only messages whose ends the tool knew (not the spawn instruction, which is `parentId`).
    if meta.from_thread.is_some() || meta.to_thread.is_some() {
        codex_inocode_team::roster_for(&thread_id.to_string()).record_message(
            meta.from_thread.as_deref(),
            meta.to_thread.as_deref(),
            delivery,
            ts_ms,
        );
    }
    codex_inocode_protocol::log_message_global(
        &thread_id.to_string(),
        &codex_inocode_protocol::LoggedMessage {
            ts_ms,
            from: from.to_string(),
            to: to.to_string(),
            delivery: delivery.to_string(),
            text: text.to_string(),
            reply_to: meta.reply_to,
            reply_thread: meta.reply_thread,
            from_thread: meta.from_thread,
            to_thread: meta.to_thread,
        },
    );
}

// ---------------------------------------------------------------------------
// Stall detection and the advisor (C5)
// ---------------------------------------------------------------------------

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_inocode_advisor::ConsultState;
use codex_inocode_advisor::Observation;
use codex_inocode_advisor::Stall;
use codex_inocode_advisor::StallConfig;
use codex_inocode_advisor::StallDetector;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

const RECENT_ACTIONS: usize = 8;

struct ThreadStall {
    detector: StallDetector,
    recent: VecDeque<String>,
    last_error: String,
}

fn detectors() -> &'static Mutex<HashMap<ThreadId, ThreadStall>> {
    static DETECTORS: OnceLock<Mutex<HashMap<ThreadId, ThreadStall>>> = OnceLock::new();
    DETECTORS.get_or_init(Default::default)
}

/// Stall thresholds from `advisor.*` parameters, read each time a detector is created.
fn stall_config() -> StallConfig {
    use codex_inocode_flags::int_param;
    let get = |key: &str| usize::try_from(int_param(key)).unwrap_or(usize::MAX);
    StallConfig {
        same_failure_k: get("advisor.same_failure_k"),
        repeat_n: get("advisor.repeat_n"),
        alternation_len: get("advisor.alternation_len"),
        no_progress_turns: get("advisor.no_progress_turns"),
    }
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// Exit code reported in a tool output, if any (`Exit code: N` or `"exit_code": N`).
pub(crate) fn exit_code_in_output(output: &str) -> Option<i32> {
    let lower = output.to_ascii_lowercase();
    for marker in ["exit code:", "\"exit_code\":", "exit_code:"] {
        if let Some(pos) = lower.find(marker) {
            let rest = lower[pos + marker.len()..].trim_start();
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse() {
                return Some(code);
            }
        }
    }
    None
}

fn test_result_in_output(output: &str) -> Option<String> {
    output
        .lines()
        .find(|line| line.contains("test result:"))
        .map(|line| truncate(line.trim(), 200))
}

/// Recording evidence is the product of a research turn: it counts as progress the way an edit
/// does for a coding turn.
fn is_evidence_record(tool: &str) -> bool {
    tool.to_ascii_lowercase().contains("record_evidence")
}

fn is_edit_tool(tool: &str) -> bool {
    let tool = tool.to_ascii_lowercase();
    tool.contains("apply_patch") || tool.contains("edit") || tool.contains("write_file")
}

pub(crate) fn build_observation(
    tool: &str,
    args: &str,
    success: bool,
    output: &str,
) -> Observation {
    let exit_code = exit_code_in_output(output);
    let nonzero_exit = exit_code.is_some_and(|c| c != 0);
    Observation {
        tool: tool.to_string(),
        args: truncate(args, 500),
        exit_code,
        error: (!success || nonzero_exit).then(|| truncate(output, 400)),
        changed_files: success && !nonzero_exit && (is_edit_tool(tool) || is_evidence_record(tool)),
        test_result: test_result_in_output(output),
        observation: codex_inocode_advisor::normalize_error(output),
        read_only: codex_inocode_advisor::is_read_only_call(tool, args),
        at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
    }
}

/// Records what the host saw a spawned child do, for the facts block of its result: the files
/// changed through edit tools and the commands run through shell tools with their exit codes.
pub(crate) fn record_facts(
    session: &Session,
    tool: &str,
    args: &str,
    success: bool,
    output: &str,
    secs: f64,
) {
    if !InocodeFlags::load().peer_messaging_enabled {
        return;
    }
    let thread = session.thread_id.to_string();
    if success && is_edit_tool(tool) {
        codex_inocode_team::record_edit(&thread, codex_inocode_team::edited_paths(args));
        return;
    }
    let name = tool.to_ascii_lowercase();
    if name.contains("exec") || name.contains("shell") || name.contains("bash") || name.contains("terminal") {
        codex_inocode_team::record_command(
            &thread,
            &codex_inocode_advisor::shell_command_of(args),
            exit_code_in_output(output),
            secs,
        );
    }
}

/// Feeds one finished tool call to the stall detector; on a stall, records it and starts a
/// background consultation whose answer arrives in the main agent's next turn input.
pub(crate) fn observe_tool_result(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    tool: &str,
    args: &str,
    success: bool,
    output: &str,
) {
    let flags = InocodeFlags::load();
    if !flags.advisor_enabled {
        return;
    }
    let obs = build_observation(tool, args, success, output);
    let summary = format!("{tool} {}", truncate(args, 160));
    let failed = obs.failed();
    let error_text = obs.error.clone().unwrap_or_default();
    let (stall, tried, last_error) = {
        let Ok(mut map) = detectors().lock() else {
            return;
        };
        let entry = map.entry(session.thread_id).or_insert_with(|| ThreadStall {
            detector: StallDetector::new(stall_config()),
            recent: VecDeque::new(),
            last_error: String::new(),
        });
        entry.recent.push_back(format!(
            "{summary} -> {}",
            if failed { "failed" } else { "ok" }
        ));
        while entry.recent.len() > RECENT_ACTIONS {
            entry.recent.pop_front();
        }
        if failed {
            entry.last_error = error_text;
        }
        let stall = entry.detector.observe(obs);
        (
            stall,
            entry.recent.iter().cloned().collect::<Vec<_>>(),
            entry.last_error.clone(),
        )
    };
    let Some(stall) = stall else { return };
    codex_inocode_advisor::record_global(
        &codex_inocode_flags::inocode_home(),
        codex_inocode_advisor::AdvisorEvent::stall(
            &session.thread_id.to_string(),
            stall.trigger,
            &stall.signature,
            tried.clone(),
        ),
    );
    if flags.metrics_enabled {
        record(
            session.thread_id,
            MetricEvent::StallDetected {
                trigger: stall.trigger.as_str().to_string(),
            },
        );
    }
    // Failure case (knowledge DB): the repeated failure signature is recorded once; the
    // store returns the existing id when a live case with the same signature exists.
    // No-op when `knowledge.enabled` is off.
    {
        let symptom = if last_error.is_empty() {
            tried.last().cloned().unwrap_or_default()
        } else {
            last_error.clone()
        };
        let cwd = crate::compact_inocode::local_cwd(turn);
        if let Err(err) = codex_inocode_knowledge::record_failure_case(
            &cwd,
            &stall.signature,
            &symptom,
            None,
            None,
            None,
            "advisor",
        ) {
            tracing::warn!("recording failure case failed: {err}");
        }
    }
    let session = Arc::clone(session);
    let turn = Arc::clone(turn);
    tokio::spawn(async move {
        run_consultation(session, turn, stall, tried, last_error).await;
    });
}

async fn last_user_text(session: &Session) -> String {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    let history = session.clone_history().await;
    for item in history.raw_items().rev() {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
        {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                return truncate(&text, 1500);
            }
        }
    }
    String::new()
}

async fn run_consultation(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    stall: Stall,
    tried: Vec<String>,
    last_error: String,
) {
    let goal = last_user_text(&session).await;
    let state = ConsultState {
        goal,
        current_work: tried.last().cloned().unwrap_or_default(),
        tried,
        failure_signature: stall.signature.clone(),
        evidence: if last_error.is_empty() {
            Vec::new()
        } else {
            vec![last_error]
        },
    };
    let model = codex_inocode_flags::string_param("advisor.model");
    let model_for_log = Some(
        model
            .clone()
            .unwrap_or_else(|| turn.model_info().slug.clone()),
    );
    let advice = codex_inocode_advisor::consult(&state, &stall, |prompt| {
        ask_advisor_model(Arc::clone(&session), Arc::clone(&turn), prompt, model)
    })
    .await;
    let thread_id = session.thread_id.to_string();
    let home = codex_inocode_flags::inocode_home();
    match advice {
        Ok(text) => {
            codex_inocode_advisor::record_global(
                &home,
                codex_inocode_advisor::AdvisorEvent::advice(
                    &thread_id,
                    stall.trigger,
                    model_for_log.as_deref(),
                    &text,
                ),
            );
            deliver_advice(&session, text).await
        }
        Err(err) => {
            tracing::warn!("advisor consultation failed: {err}");
            codex_inocode_advisor::record_global(
                &home,
                codex_inocode_advisor::AdvisorEvent::error(
                    &thread_id,
                    stall.trigger,
                    model_for_log.as_deref(),
                    &err.to_string(),
                ),
            );
        }
    }
}

/// Queues the advice as a mailbox note for the main agent's next turn input.
pub(crate) async fn deliver_advice(session: &Arc<Session>, text: String) {
    let author = codex_protocol::AgentPath::root()
        .join("advisor")
        .unwrap_or_else(|_| codex_protocol::AgentPath::root());
    let communication = codex_protocol::protocol::InterAgentCommunication::new(
        author,
        codex_protocol::AgentPath::root(),
        Vec::new(),
        text,
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication.into(), Default::default())
        .await;
}

const ADVISOR_BASE_INSTRUCTIONS: &str = "You are a debugging advisor for another engineering agent that is stuck. You do not implement anything and you do not edit files. You answer with: other hypotheses for the cause, assumptions that look suspicious, and concrete experiments that would distinguish between the hypotheses. Be brief and specific.";

/// One-shot consultation of the advisor model in an isolated sub-session
/// (collab tools off, approvals never).
async fn ask_advisor_model(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    prompt: String,
    model: Option<String>,
) -> Result<String, String> {
    use codex_features::Feature;
    use codex_protocol::models::BaseInstructionsProvenance;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::SubAgentSource;
    use codex_protocol::user_input::UserInput;

    let mut config = turn.config.as_ref().clone();
    let _ = config.features.disable(Feature::Collab);
    let _ = config.features.disable(Feature::MultiAgentV2);
    config.base_instructions = Some(ADVISOR_BASE_INSTRUCTIONS.to_string());
    config.base_instructions_provenance = Some(BaseInstructionsProvenance::Custom);
    config.permissions.approval_policy =
        crate::config::Constrained::allow_only(AskForApproval::Never);
    config.model = Some(model.unwrap_or_else(|| turn.model_info().slug.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let (_child, io) = crate::codex_delegate::run_codex_thread_one_shot(
        config,
        Arc::clone(&session.services.auth_manager),
        Arc::clone(&session.services.models_manager),
        vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }],
        Arc::clone(&session),
        Arc::clone(&turn),
        cancel,
        SubAgentSource::Other("advisor".to_string()),
        /*final_output_json_schema*/ None,
        /*initial_history*/ None,
    )
    .await
    .map_err(|err| err.to_string())?;
    while let Ok(event) = io.rx_event.recv().await {
        match event.msg {
            EventMsg::TurnComplete(done) => {
                return done
                    .last_agent_message
                    .filter(|text| !text.trim().is_empty())
                    .ok_or_else(|| "advisor returned no message".to_string());
            }
            EventMsg::TurnAborted(_) => return Err("advisor turn aborted".to_string()),
            _ => {}
        }
    }
    Err("advisor channel closed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_is_parsed_from_tool_output() {
        assert_eq!(
            exit_code_in_output("Exit code: 101\nWall time: 1s"),
            Some(101)
        );
        assert_eq!(exit_code_in_output("{\"exit_code\": 2}"), Some(2));
        assert_eq!(exit_code_in_output("hello"), None);
    }

    #[test]
    fn repeated_failures_in_tool_output_fire_the_detector() {
        let mut detector = StallDetector::new(StallConfig::default());
        let mut fired = None;
        for attempt in 0..3_u64 {
            let mut obs = build_observation(
                "shell",
                "{\"command\":\"cargo test\"}",
                true,
                "Exit code: 101\nerror: could not compile",
            );
            // Separate attempts, not one parallel batch.
            obs.at_ms = Some(10_000 + attempt * 5_000);
            fired = detector.observe(obs);
        }
        assert_eq!(
            fired.map(|s| s.trigger),
            Some(codex_inocode_advisor::Trigger::SameFailure)
        );
    }

    #[test]
    fn successful_edit_counts_as_progress() {
        let obs = build_observation("apply_patch", "{}", true, "Success");
        assert!(obs.changed_files);
        assert!(!obs.failed());
    }

    #[test]
    fn recording_evidence_counts_as_progress_and_a_search_does_not() {
        let evidence = build_observation("record_evidence", "{}", true, "recorded");
        assert!(evidence.changed_files);
        let failed = build_observation("record_evidence", "{}", false, "denied");
        assert!(!failed.changed_files);
        let search = build_observation("web_search", "{\"query\":\"x\"}", true, "results");
        assert!(!search.changed_files);
        assert!(search.read_only);
    }

    fn snapshot(limit_id: &str) -> codex_protocol::protocol::RateLimitSnapshot {
        use codex_protocol::protocol::RateLimitWindow;
        codex_protocol::protocol::RateLimitSnapshot {
            limit_id: Some(limit_id.into()),
            limit_name: None,
            normal_model_slug: None,
            primary: Some(RateLimitWindow {
                used_percent: 42.0,
                window_minutes: Some(300),
                resets_at: Some(1_794_398_400),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 18.75,
                window_minutes: Some(10_080),
                resets_at: Some(1_794_744_000),
            }),
            credits: None,
            individual_limit: None,
            spend_control_reached: None,
            plan_type: None,
            rate_limit_reached_type: None,
        }
    }

    #[test]
    fn a_claude_response_records_both_windows_under_its_own_family() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: 400,
            output_tokens: 200,
            ..Default::default()
        };
        let claude = build_request_record(
            5,
            "t",
            "claude-x",
            "anthropic",
            Some("main".into()),
            Some(&usage),
            std::time::Duration::from_secs(4),
            Some(&snapshot("claude")),
        )
        .expect("record");
        assert_eq!(claude.limit_id.as_deref(), Some("claude"));
        assert_eq!(claude.uncached_input, 600);
        assert_eq!(claude.cached_input, 400);
        assert_eq!(claude.tok_per_s, Some(50.0));
        let five = claude.five_hour.expect("5h");
        assert_eq!((five.used_percent, five.resets_at), (42.0, Some(1_794_398_400)));
        let week = claude.weekly.expect("7d");
        assert_eq!((week.used_percent, week.resets_at), (18.75, Some(1_794_744_000)));

        let codex = build_request_record(
            6,
            "t",
            "gpt-x",
            "openai",
            None,
            Some(&usage),
            std::time::Duration::from_secs(4),
            Some(&snapshot("codex")),
        )
        .expect("record");
        assert_eq!(codex.limit_id.as_deref(), Some("codex"));
        assert_ne!(codex.limit_id, claude.limit_id);

        // No usage, no record; no snapshot, no readings.
        assert!(
            build_request_record(7, "t", "m", "p", None, None, std::time::Duration::ZERO, None)
                .is_none()
        );
        let bare = build_request_record(
            8,
            "t",
            "m",
            "p",
            None,
            Some(&usage),
            std::time::Duration::from_secs(1),
            None,
        )
        .expect("record");
        assert!(bare.five_hour.is_none() && bare.weekly.is_none() && bare.limit_id.is_none());
    }
}
