//! `kb_propose`: how the model puts a failure, a page or a fact into the project's knowledge.
//! It is adopted at once (a page as a draft); the user rejects what is wrong, on the screen or by
//! telling the model in the chat (`kb_reject`).

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

pub(crate) const KB_PROPOSE_TOOL_NAME: &str = "kb_propose";

fn text_prop(description: &str) -> JsonSchema {
    JsonSchema::string(Some(description.to_string()))
}

pub(crate) fn create_kb_propose_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "kind".to_string(),
            text_prop("`page` (a readable internal write-up worth keeping, for example when the user asks you to keep something to read again later or to sum it up), `fact` (a one-line internal fact), or `failure` (something that failed). Choose `record_evidence` for external facts with a source and quote, and `kb_prohibit` for a standing ban quoting the user verbatim."),
        ),
        ("title".to_string(), text_prop("One short line.")),
        (
            "body".to_string(),
            text_prop("What to keep. For a failure: what happened and why it matters."),
        ),
        (
            "conditions".to_string(),
            text_prop("Failure only. The situation, environment and state it failed in (what ran just before, the folder, the tools available). Write `unknown` if you do not know; do not guess."),
        ),
        (
            "attempted".to_string(),
            text_prop("Failure only. What was run (the command or call)."),
        ),
        (
            "outcome".to_string(),
            text_prop("Failure only. The result: exit code and the end of the output."),
        ),
        (
            "avoid".to_string(),
            text_prop("Failure only. What to avoid or do instead, when the conditions match. Optional."),
        ),
        (
            "cause".to_string(),
            text_prop("Failure only. The cause, if you suspect one. It is kept as a hypothesis. Optional."),
        ),
        (
            "source".to_string(),
            text_prop("Where it came from: a URL or a file path. Optional."),
        ),
        (
            "checked_at".to_string(),
            text_prop("The date you checked it (YYYY-MM-DD). Optional."),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: KB_PROPOSE_TOOL_NAME.to_string(),
        description: "Add to this project's knowledge: a page (a readable internal write-up worth keeping, for example when the user asks you to keep something to read again later or to sum it up), a fact, or a failure (what was run, the result, the conditions it failed under, how to avoid it). Pick the tool by kind: `record_evidence` only for an external, verifiable fact with a verbatim quote and its URL or file path; `kb_prohibit` only for a standing ban quoting the user verbatim in this session; otherwise use this tool. Search first with `search_evidence` or `search_knowledge` to avoid duplicates. It is registered at once (a page stays a draft until the user confirms it); the user rejects what is wrong. A change to a page that exists replaces it, and the old version is kept. A failure without conditions is refused (write `unknown` if you do not know them). Something the user rejected is refused too. The result says `registered`, or `denied` with the reason."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["kind".to_string(), "title".to_string(), "body".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposeArgs {
    kind: String,
    title: String,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conditions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avoid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checked_at: Option<String>,
}

pub struct KbProposeHandler;

impl ToolExecutor<ToolInvocation> for KbProposeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(KB_PROPOSE_TOOL_NAME)
    }
    fn spec(&self) -> ToolSpec {
        create_kb_propose_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{KB_PROPOSE_TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: ProposeArgs = parse_arguments(arguments)?;
            let value = serde_json::to_value(&args).unwrap_or_default();
            let cwd = invocation.turn.cwd.as_path().to_path_buf();
            let root = codex_inocode_knowledge::kb_root(&cwd);
            let by = format!("model:{}", invocation.session.thread_id);
            let result = codex_inocode_knowledge::kb::propose(&root, &value, &by);
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for KbProposeHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_is_short_and_says_the_user_approves() {
        let ToolSpec::Function(tool) = create_kb_propose_tool() else {
            panic!("function tool expected");
        };
        assert_eq!(tool.name, "kb_propose");
        assert!(tool.description.contains("registered at once"));
        assert!(tool.description.contains("the user rejects"));
        assert!(tool.description.contains("`unknown`"));
        assert!(tool.description.chars().count() < 1100);
    }

    #[test]
    fn arguments_reject_unknown_fields() {
        assert!(serde_json::from_str::<ProposeArgs>(r#"{"kind":"fact","title":"t","body":"b","x":1}"#).is_err());
        assert!(serde_json::from_str::<ProposeArgs>(r#"{"kind":"fact","title":"t","body":"b"}"#).is_ok());
    }
}
