//! Message references: `#<turn>.<item>` (what the GUI shows next to every item) or a server item id
//! (`ThreadItem.id`), resolved against the turns built from a thread's rollout.
//!
//! The numbering is the position in the history the GUI reads: `<turn>` is the 1-based position in
//! the turn list, `<item>` the 1-based position in that turn's `items`. Every item is counted
//! (reasoning and tool calls too), so a number never moves when a kind of item is hidden.

use crate::protocol::v2::Turn;
use serde_json::Value;

/// Which item a reference names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSelector {
    /// `#<turn>.<item>`, both 1-based.
    Position { turn: usize, item: usize },
    /// `#<turn>.@<n>`: the n-th agent-to-agent message received in that turn (both 1-based).
    Mail { turn: usize, n: usize },
    /// `ThreadItem.id`.
    ItemId(String),
}

/// Parses `#12.3`, `12.3` or a server item id. A leading `#` and surrounding spaces are ignored.
pub fn parse_message_ref(text: &str) -> MessageSelector {
    let trimmed = text.trim();
    let bare = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();
    if let Some((t, i)) = bare.split_once('.') {
        if let Some(n) = i.strip_prefix('@')
            && let (Ok(turn), Ok(n)) = (t.parse::<usize>(), n.parse::<usize>())
            && turn >= 1
            && n >= 1
        {
            return MessageSelector::Mail { turn, n };
        }
        if let (Ok(turn), Ok(item)) = (t.parse::<usize>(), i.parse::<usize>())
            && turn >= 1
            && item >= 1
        {
            return MessageSelector::Position { turn, item };
        }
    }
    MessageSelector::ItemId(bare.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    /// A message from another agent.
    Agent,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Agent => "agent",
        }
    }
}

/// An item found by reference, with its full text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMessage {
    pub turn_number: usize,
    pub item_number: usize,
    pub turn_id: String,
    pub item_id: String,
    pub role: MessageRole,
    pub text: String,
    /// Sender and receiver (agent paths) of an agent-to-agent message.
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Text of an item: the words of a user or assistant message, the plan, the reasoning, or for
/// anything else (tool calls and their output) the item as JSON.
fn describe(value: &Value) -> (MessageRole, String) {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    match kind {
        "userMessage" => {
            let text = value
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| {
                            (p.get("type").and_then(Value::as_str) == Some("text"))
                                .then(|| p.get("text").and_then(Value::as_str))
                                .flatten()
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            (MessageRole::User, text)
        }
        "agentMessage" | "plan" => (
            MessageRole::Assistant,
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "reasoning" => {
            let mut parts = strings("summary");
            parts.extend(strings("content"));
            (MessageRole::Assistant, parts.join("\n"))
        }
        "commandExecution" => {
            let command = value.get("command").and_then(Value::as_str).unwrap_or("");
            let output = value
                .get("aggregatedOutput")
                .and_then(Value::as_str)
                .unwrap_or("");
            (MessageRole::Tool, format!("$ {command}\n{output}"))
        }
        _ => (
            MessageRole::Tool,
            serde_json::to_string_pretty(value).unwrap_or_default(),
        ),
    }
}

/// Finds the item `selector` names in `turns` (the same list `thread/read` returns).
pub fn resolve_message(turns: &[Turn], selector: &MessageSelector) -> Option<ResolvedMessage> {
    let found = match selector {
        MessageSelector::Position { turn, item } => {
            let t = turns.get(turn.checked_sub(1)?)?;
            let i = t.items.get(item.checked_sub(1)?)?;
            Some((*turn, *item, t, i))
        }
        // Agent messages are not items of the built history; the rollout reader finds them.
        MessageSelector::Mail { .. } => return None,
        MessageSelector::ItemId(id) => turns.iter().enumerate().find_map(|(ti, t)| {
            t.items
                .iter()
                .position(|i| i.id() == id)
                .map(|ii| (ti + 1, ii + 1, t, &t.items[ii]))
        }),
    }?;
    let (turn_number, item_number, turn, item) = found;
    let value = serde_json::to_value(&item.payload).ok()?;
    let (role, text) = describe(&value);
    Some(ResolvedMessage {
        turn_number,
        item_number,
        turn_id: turn.id.clone(),
        item_id: item.id().to_string(),
        role,
        text,
        from: None,
        to: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::v2::ThreadItem;
    use crate::protocol::v2::ThreadItemPayload;
    use crate::protocol::v2::TurnItemsView;
    use crate::protocol::v2::TurnStatus;
    use crate::protocol::v2::UserInput;

    fn turn(id: &str, items: Vec<ThreadItemPayload>) -> Turn {
        Turn {
            id: id.into(),
            root_turn_id: None,
            items: items
                .into_iter()
                .map(|payload| ThreadItem {
                    payload,
                    started_at_ms: None,
                    completed_at_ms: None,
                })
                .collect(),
            items_view: TurnItemsView::default(),
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }

    fn user(id: &str, text: &str) -> ThreadItemPayload {
        ThreadItemPayload::UserMessage {
            id: id.into(),
            client_id: None,
            content: vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }],
        }
    }

    fn agent(id: &str, text: &str) -> ThreadItemPayload {
        ThreadItemPayload::AgentMessage {
            moderation: None,
            id: id.into(),
            text: text.into(),
            phase: None,
            memory_citation: None,
            delivery: None,
            questions: None,
        }
    }

    fn turns() -> Vec<Turn> {
        vec![
            turn("t1", vec![user("u1", "hello"), agent("a1", "hi there")]),
            turn(
                "t2",
                vec![
                    user("u2", "SQLite の並行書き込みは?"),
                    ThreadItemPayload::Reasoning {
                        id: "r2".into(),
                        summary: vec!["thinking".into()],
                        content: Vec::new(),
                    },
                    agent("a2", "WAL を使います。"),
                ],
            ),
        ]
    }

    #[test]
    fn references_are_parsed() {
        assert_eq!(
            parse_message_ref("#12.3"),
            MessageSelector::Position { turn: 12, item: 3 }
        );
        assert_eq!(
            parse_message_ref(" 2.1 "),
            MessageSelector::Position { turn: 2, item: 1 }
        );
        assert_eq!(
            parse_message_ref("#3.@2"),
            MessageSelector::Mail { turn: 3, n: 2 }
        );
        assert_eq!(
            parse_message_ref("#3.@0"),
            MessageSelector::ItemId("3.@0".into())
        );
        assert_eq!(
            parse_message_ref("#0.1"),
            MessageSelector::ItemId("0.1".into())
        );
        assert_eq!(
            parse_message_ref("msg_abc"),
            MessageSelector::ItemId("msg_abc".into())
        );
    }

    #[test]
    fn a_position_and_an_item_id_name_the_same_item() {
        let t = turns();
        let by_pos = resolve_message(&t, &parse_message_ref("#2.3")).expect("by position");
        assert_eq!(by_pos.item_id, "a2");
        assert_eq!(by_pos.turn_id, "t2");
        assert_eq!(by_pos.role, MessageRole::Assistant);
        assert_eq!(by_pos.text, "WAL を使います。");
        let by_id = resolve_message(&t, &parse_message_ref("a2")).expect("by id");
        assert_eq!(by_id, by_pos);
        assert_eq!((by_id.turn_number, by_id.item_number), (2, 3));
    }

    #[test]
    fn hidden_kinds_still_take_a_number() {
        let t = turns();
        let r = resolve_message(&t, &parse_message_ref("#2.2")).expect("reasoning");
        assert_eq!(r.item_id, "r2");
        assert_eq!(r.text, "thinking");
        let u = resolve_message(&t, &parse_message_ref("#2.1")).expect("user");
        assert_eq!(u.role, MessageRole::User);
        assert_eq!(u.text, "SQLite の並行書き込みは?");
    }

    #[test]
    fn a_missing_reference_is_none() {
        let t = turns();
        assert!(resolve_message(&t, &parse_message_ref("#3.1")).is_none());
        assert!(resolve_message(&t, &parse_message_ref("#1.9")).is_none());
        assert!(resolve_message(&t, &parse_message_ref("nope")).is_none());
        assert!(resolve_message(&[], &parse_message_ref("#1.1")).is_none());
    }

    #[test]
    fn items_before_a_compaction_stay_readable() {
        use crate::protocol::thread_history::build_turns_from_rollout_items;
        use codex_protocol::protocol::AgentMessageEvent;
        use codex_protocol::protocol::EventMsg;
        use codex_protocol::protocol::UserMessageEvent;
        use codex_rollout::CompactedItem;
        use codex_rollout::RolloutItem;

        let user_msg = |text: &str| {
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: text.into(),
                images: None,
                text_elements: Vec::new(),
                local_images: Vec::new(),
                ..Default::default()
            }))
        };
        let agent_msg = |text: &str| {
            RolloutItem::EventMsg(EventMsg::AgentMessage(AgentMessageEvent {
                moderation: None,
                message: text.into(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            }))
        };
        let items = vec![
            user_msg("First"),
            agent_msg("Reply one"),
            RolloutItem::Compacted(Box::new(CompactedItem {
                message: String::new(),
                replacement_history: None,
                retained_context: None,
                guardian_history: None,
                mcp_resource_origins: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                compaction_response_id: None,
                latest_token_usage_record: None,
                resume_metadata: None,
            })),
            user_msg("Second"),
            agent_msg("Reply two"),
        ];
        let turns = build_turns_from_rollout_items(&items);
        let old = resolve_message(&turns, &parse_message_ref("#1.2")).expect("old reply");
        assert_eq!(old.text, "Reply one");
        let by_id = resolve_message(&turns, &parse_message_ref(&old.item_id)).expect("by id");
        assert_eq!(by_id.text, "Reply one");
        let new = resolve_message(&turns, &parse_message_ref("#2.2")).expect("new reply");
        assert_eq!(new.text, "Reply two");
    }
}
