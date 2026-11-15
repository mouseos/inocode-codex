//! The InoCode thread an imported session becomes (Legacy history mode).
//!
//! What the screen shows comes from the events: the user's and the agent's messages, the agent's
//! thinking, and every tool call with its whole input and output (as an MCP-style call of the
//! "Claude Code" or "Codex" server, the one kind of tool call a Legacy rollout keeps). What a model
//! reads when the conversation goes on comes from the response items: the messages, and each tool
//! call as a short note (a whole tool output does not go back into the context).

use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_rollout::RolloutItem;
use serde_json::Value;
use std::time::Duration;

/// How much of a tool call's input and output goes into the model's context as a note.
const NOTE_INPUT_CHARS: usize = 2_000;
const NOTE_OUTPUT_CHARS: usize = 4_000;

pub(crate) fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n[… {} characters more]", text.chars().count() - max)
}

pub(crate) struct Builder {
    server: &'static str,
    items: Vec<RolloutItem>,
    turn: Option<(String, Option<i64>)>,
    turns: usize,
    calls: usize,
    bytes: i64,
    last_ts: Option<i64>,
    pub(crate) user_texts: Vec<String>,
    pub(crate) first_user: Option<String>,
}

impl Builder {
    /// `server`: the name the tool calls are shown under ("Claude Code", "Codex").
    pub(crate) fn new(server: &'static str) -> Self {
        Self {
            server,
            items: Vec::new(),
            turn: None,
            turns: 0,
            calls: 0,
            bytes: 0,
            last_ts: None,
            user_texts: Vec::new(),
            first_user: None,
        }
    }

    fn close_turn(&mut self, completed_at: Option<i64>) {
        if let Some((turn_id, started_at)) = self.turn.take() {
            self.items.push(RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                root_turn_id: None,
                turn_id,
                last_agent_message: None,
                error: None,
                started_at,
                completed_at,
                duration_ms: None,
                time_to_first_token_ms: None,
            })));
        }
    }

    fn ensure_turn(&mut self, ts: Option<i64>) {
        if self.turn.is_none() {
            self.open_turn(ts);
        }
    }

    fn open_turn(&mut self, ts: Option<i64>) {
        self.turns += 1;
        let turn_id = format!("inocode-import-turn-{}", self.turns);
        self.items.push(RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
            turn_attribution: None,
            turn_id: turn_id.clone(),
            root_turn_id: None,
            trace_id: None,
            started_at: ts,
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        })));
        self.turn = Some((turn_id, ts));
    }

    fn note_ts(&mut self, ts: Option<i64>) {
        if ts.is_some() {
            self.last_ts = ts;
        }
    }

    pub(crate) fn user(&mut self, text: &str, ts: Option<i64>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let last = self.last_ts;
        self.close_turn(last);
        self.note_ts(ts);
        self.open_turn(ts);
        self.items.push(RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
            message: text.to_string(),
            ..Default::default()
        })));
        self.push_message("user", text);
        self.user_texts.push(text.to_string());
        if self.first_user.is_none() {
            self.first_user = Some(text.to_string());
        }
    }

    pub(crate) fn agent(&mut self, text: &str, ts: Option<i64>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.note_ts(ts);
        self.ensure_turn(ts);
        self.items.push(RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
            moderation: None,
            message: text.to_string(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        })));
        self.push_message("assistant", text);
    }

    pub(crate) fn reasoning(&mut self, text: &str, ts: Option<i64>) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.note_ts(ts);
        self.ensure_turn(ts);
        self.items.push(RolloutItem::EventMsg(EventMsg::AgentReasoning(AgentReasoningEvent {
            text: text.to_string(),
        })));
    }

    /// A tool call with its whole input and output (`output` None: no result was recorded).
    pub(crate) fn tool(&mut self, name: &str, input: Value, output: Option<(String, bool)>, ts: Option<i64>) {
        self.note_ts(ts);
        self.ensure_turn(ts);
        self.calls += 1;
        let turn_id = self.turn.as_ref().map(|t| t.0.clone()).unwrap_or_default();
        let (text, failed) = output.clone().unwrap_or_default();
        let result = if failed {
            Err(text.clone())
        } else {
            Ok(CallToolResult {
                content: vec![serde_json::json!({ "type": "text", "text": text })],
                structured_content: None,
                is_error: None,
                meta: None,
            })
        };
        self.items.push(RolloutItem::EventMsg(EventMsg::McpToolCallEnd(McpToolCallEndEvent {
            call_id: format!("inocode-import-call-{}", self.calls),
            turn_id,
            invocation: McpInvocation {
                server: self.server.to_string(),
                tool: name.to_string(),
                arguments: Some(input.clone()),
            },
            connector_id: None,
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            link_id: None,
            app_name: None,
            action_name: None,
            plugin_id: None,
            read_only_hint: None,
            duration: Duration::ZERO,
            result,
        })));
        let input_text = match &input {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut note = format!("[{} tool call: {name}]\n{}", self.server, cut(&input_text, NOTE_INPUT_CHARS));
        if let Some((out, failed)) = output {
            let label = if failed { "error" } else { "result" };
            note.push_str(&format!("\n[{label}]\n{}", cut(&out, NOTE_OUTPUT_CHARS)));
        }
        self.push_message("assistant", &note);
    }

    fn push_message(&mut self, role: &str, text: &str) {
        self.bytes = self.bytes.saturating_add(i64::try_from(text.len()).unwrap_or(i64::MAX));
        let content = if role == "user" {
            ContentItem::InputText { text: text.to_string() }
        } else {
            ContentItem::OutputText { annotations: None, logprobs: None, text: text.to_string() }
        };
        self.items.push(RolloutItem::ResponseItem(
            ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: role.to_string(),
                content: vec![content],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }
            .into(),
        ));
    }

    pub(crate) fn finish(mut self) -> (Vec<RolloutItem>, Vec<String>, Option<String>, i64) {
        if self.turn.is_some() {
            // Roughly 4 bytes a token: what the context meter shows for the imported history.
            let tokens = self.bytes / 4;
            let usage = TokenUsage { total_tokens: tokens, ..TokenUsage::default() };
            self.items.push(RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: usage.clone(),
                    last_usage_includes_reasoning: None,
                    last_token_usage: usage,
                    model_context_window: None,
                }),
                rate_limits: None,
            })));
        }
        let last = self.last_ts;
        self.close_turn(last);
        let tokens = self.bytes / 4;
        (self.items, self.user_texts, self.first_user, tokens)
    }
}
