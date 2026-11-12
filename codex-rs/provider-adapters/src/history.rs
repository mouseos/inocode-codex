//! Responses input items → [`Turn`]s (replaces OpenCodex `responses/parser.ts`: InoCode receives
//! typed items, so only the grouping rules are needed).
//!
//! * `developer` / `system` messages become [`Turn::System`].
//! * Consecutive assistant output (reasoning, assistant messages, function calls) is one
//!   [`Turn::Assistant`], in order.
//! * A `function_call_output` becomes a [`Turn::ToolResult`].
//! * A message between agents becomes a user message that says who sent it.
//! * Items that have no wire form (additional tools and the like) are dropped.

use crate::envelope::Envelope;
use crate::ir::AssistantPart;
use crate::ir::Part;
use crate::ir::Thinking;
use crate::ir::ToolCall;
use crate::ir::ToolResult;
use crate::ir::Turn;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;

/// What the model is told a search loaded: the names it can now call, with their descriptions.
fn loaded_summary(tools: &[serde_json::Value]) -> String {
    let mut lines = Vec::new();
    for decl in crate::tool_names::loaded_tools(tools) {
        let description: String = decl.description.chars().take(160).collect();
        lines.push(format!("- {}: {}", decl.wire_name, description));
    }
    if lines.is_empty() {
        "No matching tools.".to_string()
    } else {
        format!("These tools are now available to call:
{}", lines.join("
"))
    }
}

pub fn turns_from_items(items: &[ResponseItem]) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut assistant: Vec<AssistantPart> = Vec::new();
    let flush = |turns: &mut Vec<Turn>, assistant: &mut Vec<AssistantPart>| {
        if !assistant.is_empty() {
            turns.push(Turn::Assistant(std::mem::take(assistant)));
        }
    };

    for item in items {
        match item {
            ResponseItem::Message { role, content, .. } => match role.as_str() {
                "assistant" => {
                    let text = join_text(content);
                    if !text.is_empty() {
                        assistant.push(AssistantPart::Text(text));
                    }
                }
                "developer" | "system" => {
                    flush(&mut turns, &mut assistant);
                    let text = join_text(content);
                    if !text.is_empty() {
                        turns.push(Turn::System(text));
                    }
                }
                _ => {
                    flush(&mut turns, &mut assistant);
                    let parts = user_parts(content);
                    if !parts.is_empty() {
                        turns.push(Turn::User(parts));
                    }
                }
            },
            ResponseItem::Reasoning {
                summary,
                content,
                encrypted_content,
                ..
            } => {
                let envelope = encrypted_content
                    .as_deref()
                    .and_then(Envelope::decode)
                    .unwrap_or_default();
                let mut text = String::new();
                for part in content.iter().flatten() {
                    match part {
                        ReasoningItemContent::ReasoningText { text: t }
                        | ReasoningItemContent::Text { text: t } => text.push_str(t),
                    }
                }
                if text.is_empty() {
                    for ReasoningItemReasoningSummary::SummaryText { text: t } in summary {
                        text.push_str(t);
                    }
                }
                if text.is_empty()
                    && let Some(hidden) = &envelope.txt
                {
                    text.clone_from(hidden);
                }
                if !text.is_empty() || envelope.sig.is_some() || !envelope.red.is_empty() {
                    assistant.push(AssistantPart::Thinking(Thinking {
                        text,
                        signature: envelope.sig,
                        redacted: envelope.red,
                    }));
                }
            }
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => assistant.push(AssistantPart::Call(ToolCall {
                id: call_id.clone(),
                namespace: namespace.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            })),
            ResponseItem::CustomToolCall {
                call_id,
                name,
                namespace,
                input,
                ..
            } => assistant.push(AssistantPart::Call(ToolCall {
                id: call_id.clone(),
                namespace: namespace.clone(),
                name: name.clone(),
                arguments: serde_json::json!({ "input": input }).to_string(),
            })),
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                arguments,
                ..
            } => assistant.push(AssistantPart::Call(ToolCall {
                id: call_id.clone(),
                namespace: None,
                name: crate::tool_names::TOOL_SEARCH_NAME.to_string(),
                arguments: arguments.to_string(),
            })),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                tools,
                ..
            } => {
                flush(&mut turns, &mut assistant);
                turns.push(Turn::ToolResult(ToolResult {
                    call_id: call_id.clone(),
                    name: Some(crate::tool_names::TOOL_SEARCH_NAME.to_string()),
                    content: vec![Part::Text(loaded_summary(tools))],
                    is_error: false,
                }));
            }
            ResponseItem::FunctionCallOutput {
                call_id,
                name,
                output,
                ..
            } => {
                flush(&mut turns, &mut assistant);
                turns.push(Turn::ToolResult(ToolResult {
                    call_id: call_id.clone().unwrap_or_default(),
                    name: name.clone(),
                    content: output_parts(&output.body),
                    is_error: output.success == Some(false),
                }));
            }
            ResponseItem::CustomToolCallOutput {
                call_id,
                name,
                output,
                ..
            } => {
                flush(&mut turns, &mut assistant);
                turns.push(Turn::ToolResult(ToolResult {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: output_parts(&output.body),
                    is_error: output.success == Some(false),
                }));
            }
            // A message between agents (an advisor's note, a reply, the host's facts) reaches these
            // providers as a user message that says who sent it. Encrypted text cannot be read here.
            ResponseItem::AgentMessage {
                author,
                recipient,
                content,
                ..
            } => {
                let text: Vec<&str> = content
                    .iter()
                    .filter_map(|part| match part {
                        AgentMessageInputContent::InputText { text } => Some(text.as_str()),
                        AgentMessageInputContent::EncryptedContent { .. } => None,
                    })
                    .collect();
                if !text.is_empty() {
                    flush(&mut turns, &mut assistant);
                    turns.push(Turn::User(vec![Part::Text(format!(
                        "[message from {author} to {recipient}]
{}",
                        text.join("
")
                    ))]));
                }
            }
            _ => {}
        }
    }
    flush(&mut turns, &mut assistant);
    turns
}

fn join_text(content: &[ContentItem]) -> String {
    let mut text = String::new();
    for item in content {
        match item {
            ContentItem::InputText { text: t } | ContentItem::OutputText { text: t, .. } => {
                text.push_str(t);
            }
            _ => {}
        }
    }
    text
}

fn user_parts(content: &[ContentItem]) -> Vec<Part> {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    parts.push(Part::Text(text.clone()));
                }
            }
            ContentItem::InputImage { image, .. } => match image {
                ImageReference::Inline { image_url } => parts.push(Part::Image(image_url.clone())),
                ImageReference::File { .. } => parts.push(Part::Text("[image file]".to_string())),
            },
            ContentItem::InputAudio { .. } => parts.push(Part::Text("[audio]".to_string())),
        }
    }
    parts
}

fn output_parts(body: &FunctionCallOutputBody) -> Vec<Part> {
    match body {
        FunctionCallOutputBody::Text(text) => vec![Part::Text(text.clone())],
        FunctionCallOutputBody::ContentItems(items) => items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(Part::Text(text.clone())),
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url },
                    ..
                } => Some(Part::Image(image_url.clone())),
                FunctionCallOutputContentItem::InputImage { .. } => {
                    Some(Part::Text("[image file]".to_string()))
                }
                FunctionCallOutputContentItem::InputAudio { .. } => {
                    Some(Part::Text("[audio]".to_string()))
                }
                FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn items(value: serde_json::Value) -> Vec<ResponseItem> {
        serde_json::from_value(value).expect("items")
    }

    #[test]
    fn a_message_between_agents_becomes_a_user_message() {
        let turns = turns_from_items(&items(json!([
            {"type": "agent_message", "author": "/root/advisor", "recipient": "/root",
             "content": [{"type": "input_text", "text": "try another way"}]},
            {"type": "agent_message", "author": "/root/x", "recipient": "/root",
             "content": [{"type": "encrypted_content", "encrypted_content": "zz"}]}
        ])));
        assert_eq!(
            turns,
            vec![Turn::User(vec![Part::Text(
                "[message from /root/advisor to /root]
try another way".to_string()
            )])]
        );
    }

    #[test]
    fn groups_assistant_output_and_pairs_results() {
        let turns = turns_from_items(&items(json!([
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "rules"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": null},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "calling"}]},
            {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_a"},
            {"type": "function_call_output", "call_id": "call_a", "output": "files"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
        ])));
        assert_eq!(turns.len(), 5);
        assert_eq!(turns[0], Turn::System("rules".to_string()));
        assert_eq!(turns[1], Turn::User(vec![Part::Text("hi".to_string())]));
        let Turn::Assistant(parts) = &turns[2] else {
            panic!("expected assistant");
        };
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[2], AssistantPart::Call(c) if c.id == "call_a"));
        let Turn::ToolResult(result) = &turns[3] else {
            panic!("expected tool result");
        };
        assert_eq!(result.content, vec![Part::Text("files".to_string())]);
    }

    #[test]
    fn a_signed_reasoning_item_keeps_its_signature() {
        let encrypted = Envelope {
            sig: Some("sig-1".to_string()),
            ..Envelope::default()
        }
        .encode();
        let turns = turns_from_items(&items(json!([
            {"type": "reasoning", "summary": [], "encrypted_content": encrypted}
        ])));
        let Turn::Assistant(parts) = &turns[0] else {
            panic!("expected assistant");
        };
        assert!(matches!(&parts[0],
            AssistantPart::Thinking(t) if t.signature.as_deref() == Some("sig-1")));
    }

    #[test]
    fn a_tool_search_round_trips_as_a_call_and_a_result_that_names_the_loaded_tools() {
        let items = vec![
            ResponseItem::ToolSearchCall {
                encrypted_content: None,
                id: None,
                call_id: Some("c1".into()),
                status: None,
                execution: "client".into(),
                arguments: serde_json::json!({"query": "files"}),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("c1".into()),
                status: "completed".into(),
                execution: "client".into(),
                tools: vec![serde_json::json!({
                    "type": "namespace", "name": "mcp__files__", "description": "",
                    "tools": [{"type": "function", "name": "read", "description": "Read a file",
                               "strict": false, "parameters": {"type": "object", "properties": {}}}]
                })],
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let turns = turns_from_items(&items);
        assert_eq!(turns.len(), 2);
        let Turn::Assistant(parts) = &turns[0] else { panic!("assistant turn") };
        let AssistantPart::Call(call) = &parts[0] else { panic!("call") };
        assert_eq!(call.name, "tool_search");
        assert_eq!(call.arguments, "{\"query\":\"files\"}");
        let Turn::ToolResult(result) = &turns[1] else { panic!("result") };
        assert_eq!(result.call_id, "c1");
        let Part::Text(text) = &result.content[0] else { panic!("text") };
        assert!(text.contains("mcp__files__read"));
    }
}
