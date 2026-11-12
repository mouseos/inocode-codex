//! A Responses request to a provider that is not OpenAI (a provider configured by hand, a local
//! server), made to fit what such a server takes.
//!
//! OpenAI's Responses API has tool kinds others do not: `namespace` (a group of tools), the
//! client-run `tool_search` that loads tools on demand, and freeform `custom` tools. Declared as
//! they are, every connector and MCP tool is sent on every request (hundreds of thousands of
//! tokens). Here they travel as plain functions, the way the chat and messages wires carry them:
//!
//! - `tool_search` is a function; tools it loaded earlier are declared again, flat (`namespace`
//!   + name joined), and a freeform tool is a function with one `input` string;
//! - history items of those kinds become `function_call` / `function_call_output`;
//! - on the way back a call to those functions becomes the item the harness expects.
//!
//! Nothing here is applied to OpenAI itself.

use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use codex_protocol::models::ResponseItem;
use codex_provider_adapters::ir::ToolDecl;
use codex_provider_adapters::tool_names::TOOL_SEARCH_NAME;
use codex_provider_adapters::tool_names::ToolRegistry;
use codex_provider_adapters::tool_names::custom_tool_names;
use codex_provider_adapters::tool_names::declared_tools;
use codex_provider_adapters::tool_names::loaded_tools;
use codex_provider_adapters::tool_names::wire_name;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use tokio::sync::mpsc;

/// What the way back needs to know about the request that went out.
#[derive(Clone, Debug, Default)]
pub struct Bridge {
    registry: ToolRegistry,
    custom: HashSet<String>,
}

fn function_json(decl: &ToolDecl) -> Value {
    json!({
        "type": "function",
        "name": decl.wire_name,
        "description": decl.description,
        "parameters": decl.parameters,
        "strict": decl.strict,
    })
}

fn output_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Rewrites the request body in place and returns what the response needs.
pub fn apply(body: &mut Value) -> Bridge {
    let tools_before = body.get("tools").cloned().unwrap_or(Value::Null);
    let custom = custom_tool_names(&tools_before);

    // Tools a search loaded earlier in this thread are declared again.
    let mut loaded: Vec<Value> = Vec::new();
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input.iter_mut() {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            match kind.as_str() {
                "tool_search_call" => {
                    let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    *item = json!({
                        "type": "function_call",
                        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "name": TOOL_SEARCH_NAME,
                        "arguments": output_text(&arguments),
                    });
                }
                "tool_search_output" => {
                    let tools: Vec<Value> = item
                        .get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    // As the reference implementation (opencodex `tool-search-compat.ts`) does:
                    // the output is the JSON of the loaded tools.
                    let mut payload = json!({ "tools": tools });
                    if let Some(status) = item.get("status").and_then(Value::as_str) {
                        payload["status"] = Value::String(status.to_string());
                    }
                    let summary = payload.to_string();
                    loaded.extend(tools);
                    *item = json!({
                        "type": "function_call_output",
                        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "output": summary,
                    });
                }
                "function_call" => {
                    let namespace = item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .filter(|ns| !ns.is_empty())
                        .map(str::to_string);
                    if let Some(object) = item.as_object_mut() {
                        if let Some(namespace) = namespace {
                            let name = object
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            object.insert("name".into(), Value::String(wire_name(Some(&namespace), &name)));
                        }
                        object.remove("namespace");
                    }
                }
                "custom_tool_call" => {
                    let text = item.get("input").and_then(Value::as_str).unwrap_or("").to_string();
                    *item = json!({
                        "type": "function_call",
                        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": json!({"input": text}).to_string(),
                    });
                }
                "custom_tool_call_output" => {
                    let output = item.get("output").map(output_text).unwrap_or_default();
                    *item = json!({
                        "type": "function_call_output",
                        "call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                        "output": output,
                    });
                }
                _ => {}
            }
        }
    }

    // The tool list: plain functions stay; namespaces, the search and freeform tools are flattened
    // into functions; other kinds (hosted tools) stay as they are.
    let mut declared: Vec<ToolDecl> = Vec::new();
    let mut new_tools: Vec<Value> = Vec::new();
    if let Some(list) = tools_before.as_array() {
        for tool in list {
            match tool.get("type").and_then(Value::as_str) {
                Some("function") => {
                    // `defer_loading` is OpenAI's marker; other servers do not take it.
                    let mut plain = tool.clone();
                    if let Some(object) = plain.as_object_mut() {
                        object.remove("defer_loading");
                    }
                    new_tools.push(plain);
                    declared.extend(declared_tools(&Value::Array(vec![tool.clone()])));
                }
                Some("namespace" | "tool_search" | "custom") => {
                    for decl in declared_tools(&Value::Array(vec![tool.clone()])) {
                        new_tools.push(function_json(&decl));
                        declared.push(decl);
                    }
                }
                _ => new_tools.push(tool.clone()),
            }
        }
    }
    for decl in loaded_tools(&loaded) {
        if !declared.iter().any(|known| known.wire_name == decl.wire_name) {
            new_tools.push(function_json(&decl));
            declared.push(decl);
        }
    }
    if body.get("tools").is_some() {
        body["tools"] = Value::Array(new_tools);
    }
    // A forced search names the function now.
    if body
        .get("tool_choice")
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool_search")
    {
        body["tool_choice"] = json!({"type": "function", "name": TOOL_SEARCH_NAME});
    }
    Bridge {
        registry: ToolRegistry::new(&declared),
        custom,
    }
}

impl Bridge {
    /// A call to one of the functions that stand for something else becomes that again.
    pub fn restore(&self, item: ResponseItem) -> ResponseItem {
        let item = crate::endpoint::wire::search_call(item, &self.custom);
        match item {
            ResponseItem::FunctionCall {
                encrypted_content,
                status,
                id,
                name,
                namespace: None,
                arguments,
                encrypted_function_args,
                call_id,
                internal_chat_message_metadata_passthrough,
            } => match self.registry.resolve(&name) {
                Some((Some(namespace), bare)) => ResponseItem::FunctionCall {
                    encrypted_content,
                    status,
                    id,
                    name: bare,
                    namespace: Some(namespace),
                    arguments,
                    encrypted_function_args,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                },
                _ => ResponseItem::FunctionCall {
                    encrypted_content,
                    status,
                    id,
                    name,
                    namespace: None,
                    arguments,
                    encrypted_function_args,
                    call_id,
                    internal_chat_message_metadata_passthrough,
                },
            },
            other => other,
        }
    }

    fn map(&self, event: ResponseEvent) -> Option<ResponseEvent> {
        match event {
            ResponseEvent::OutputItemDone(item) => Some(ResponseEvent::OutputItemDone(self.restore(item))),
            ResponseEvent::OutputItemAdded(item) => Some(ResponseEvent::OutputItemAdded(self.restore(item))),
            // The argument text of a freeform tool arrives wrapped in a JSON object; the finished
            // item carries the text itself.
            ResponseEvent::ToolCallInputDelta { call_id, .. } if call_id.is_some() => None,
            other => Some(other),
        }
    }

    /// The response stream with the way back applied to every item.
    pub fn wrap(self, mut stream: ResponseStream) -> ResponseStream {
        let (tx, rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
        let routing_affinity = stream.routing_affinity.take();
        let upstream_request_id = stream.upstream_request_id.take();
        let interrupt = stream.interrupt.take();
        tokio::spawn(async move {
            while let Some(event) = stream.rx_event.recv().await {
                let mapped = match event {
                    Ok(event) => match self.map(event) {
                        Some(event) => Ok(event),
                        None => continue,
                    },
                    Err(error) => Err(error),
                };
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        ResponseStream {
            routing_affinity,
            rx_event: rx,
            upstream_request_id,
            interrupt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ResponseItem;

    fn request() -> Value {
        json!({
            "model": "m",
            "tools": [
                {"type": "function", "name": "shell", "description": "run", "strict": false,
                 "parameters": {"type": "object", "properties": {}}},
                {"type": "tool_search", "execution": "client", "description": "Find tools",
                 "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}},
                {"type": "custom", "name": "apply_patch", "description": "Edit files",
                 "format": {"type": "grammar", "syntax": "lark", "definition": "start: x"}},
                {"type": "web_search"}
            ],
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "tool_search_call", "call_id": "c1", "execution": "client", "arguments": {"query": "notion"}},
                {"type": "tool_search_output", "call_id": "c1", "execution": "client", "tools": [
                    {"type": "namespace", "name": "mcp__notion__", "description": "",
                     "tools": [{"type": "function", "name": "search", "description": "Search pages", "strict": false,
                                "parameters": {"type": "object", "properties": {}}}]}
                ]},
                {"type": "function_call", "call_id": "c2", "namespace": "mcp__notion__", "name": "search", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c2", "output": "found"},
                {"type": "custom_tool_call", "call_id": "c3", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "c3", "output": "ok"}
            ]
        })
    }

    fn names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| t["type"].as_str().unwrap_or("?").to_string(), str::to_string)
            })
            .collect()
    }

    #[test]
    fn the_request_carries_plain_functions_and_redeclares_what_a_search_loaded() {
        let mut body = request();
        let _bridge = apply(&mut body);
        assert_eq!(
            names(&body),
            ["shell", "tool_search", "apply_patch", "web_search", "mcp__notion__search"]
        );
        let kinds: Vec<&str> = body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["type"].as_str().unwrap_or("?"))
            .collect();
        assert_eq!(kinds, ["function", "function", "function", "web_search", "function"]);
        let input = body["input"].as_array().expect("input");
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap_or("?")).collect();
        assert_eq!(
            types,
            ["message", "function_call", "function_call_output", "function_call", "function_call_output", "function_call", "function_call_output"]
        );
        assert_eq!(input[1]["name"], "tool_search");
        assert_eq!(input[1]["arguments"], "{\"query\":\"notion\"}");
        let output: Value =
            serde_json::from_str(input[2]["output"].as_str().expect("text")).expect("json");
        assert_eq!(output["tools"][0]["name"], "mcp__notion__");
        assert_eq!(input[3]["name"], "mcp__notion__search");
        assert!(input[3].get("namespace").is_none());
        assert_eq!(input[5]["name"], "apply_patch");
        assert_eq!(input[5]["arguments"], "{\"input\":\"*** Begin Patch\"}");
    }

    fn call(name: &str, arguments: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            encrypted_content: None,
            status: None,
            id: None,
            name: name.to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            encrypted_function_args: None,
            call_id: "x".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn a_call_to_those_functions_comes_back_as_what_it_stands_for() {
        let mut body = request();
        let bridge = apply(&mut body);
        assert!(matches!(
            bridge.restore(call("tool_search", "{\"query\":\"a\"}")),
            ResponseItem::ToolSearchCall { .. }
        ));
        match bridge.restore(call("apply_patch", "{\"input\":\"text\"}")) {
            ResponseItem::CustomToolCall { input, name, .. } => {
                assert_eq!((input.as_str(), name.as_str()), ("text", "apply_patch"));
            }
            other => panic!("{other:?}"),
        }
        // A flat name the request declared (a loaded tool) is split back into its namespace.
        let mut with_loaded = request();
        let loaded_bridge = apply(&mut with_loaded);
        match loaded_bridge.restore(call("mcp__notion__search", "{}")) {
            ResponseItem::FunctionCall { name, namespace, .. } => {
                assert_eq!((name.as_str(), namespace.as_deref()), ("search", Some("mcp__notion__")));
            }
            other => panic!("{other:?}"),
        }
        // A plain function and an unknown name stay as they are.
        assert!(matches!(
            bridge.restore(call("shell", "{}")),
            ResponseItem::FunctionCall { namespace: None, .. }
        ));
        assert!(matches!(
            bridge.restore(call("undeclared", "{}")),
            ResponseItem::FunctionCall { namespace: None, .. }
        ));
    }

    #[test]
    fn the_deferred_marker_is_dropped_and_a_forced_search_names_the_function() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "a", "defer_loading": true,
                       "parameters": {"type": "object", "properties": {}}}],
            "tool_choice": {"type": "tool_search"},
            "input": []
        });
        let _ = apply(&mut body);
        assert!(body["tools"][0].get("defer_loading").is_none());
        assert_eq!(body["tool_choice"], json!({"type": "function", "name": "tool_search"}));
    }

    #[test]
    fn a_request_without_tools_is_left_alone() {
        let mut body = json!({"model": "m", "input": []});
        let _ = apply(&mut body);
        assert!(body.get("tools").is_none());
    }
}
