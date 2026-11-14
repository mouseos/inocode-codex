//! `kb_prohibit`: the user says in the chat that something must not be done in this project
//! ("from now on, never delete out/"), and the model records it as a prohibition, quoting the
//! user's words. The guard then stops the operation. It is registered at once; the user takes it
//! out with "retract" in the knowledge tab or by saying so in the chat (`kb_reject`).

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
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const KB_PROHIBIT_TOOL_NAME: &str = "kb_prohibit";

fn list(description: &str) -> JsonSchema {
    JsonSchema::array(JsonSchema::string(None), Some(description.to_string()))
}

pub(crate) fn create_kb_prohibit_tool() -> ToolSpec {
    let matching = BTreeMap::from([
        (
            "commands".to_string(),
            list("Commands it stops: a glob, or `re:` and a regular expression (for example `re:(Remove-Item|rm|rmdir|del)\\b.*\\bout\\b`)."),
        ),
        ("paths".to_string(), list("Files or folders it protects from being written or deleted (globs, for example `out/**`).")),
        ("tools".to_string(), list("Tool names it stops (globs). Optional.")),
    ]);
    let properties = BTreeMap::from([
        ("title".to_string(), JsonSchema::string(Some("One short line: what must not be done.".to_string()))),
        (
            "quote".to_string(),
            JsonSchema::string(Some(
                "The user's own words that say it, copied exactly from their message in this session.".to_string(),
            )),
        ),
        ("reason".to_string(), JsonSchema::string(Some("Why, if the user said why. Optional.".to_string()))),
        (
            "strength".to_string(),
            JsonSchema::string(Some("`block` (stop it; the default) or `warn` (allow it with a warning).".to_string())),
        ),
        (
            "match".to_string(),
            JsonSchema::object(matching, None, /*additional_properties*/ Some(false.into())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: KB_PROHIBIT_TOOL_NAME.to_string(),
        description: "Record a prohibition the user states for this project (\"from now on, don't ...\", \"never ...\" — a standing rule, not a one-time instruction), quoting their words. Give `match` so it can be enforced: the operations matching it are then stopped before they run, in this session and later ones. It is registered at once; the user can retract it. The quote must be word for word in what the user typed in this session. The result says `registered`, or `denied` with the reason."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["title".to_string(), "quote".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProhibitArgs {
    title: String,
    quote: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    strength: Option<String>,
    #[serde(default, rename = "match")]
    match_spec: Option<serde_json::Value>,
}

pub struct KbProhibitHandler;

impl ToolExecutor<ToolInvocation> for KbProhibitHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(KB_PROHIBIT_TOOL_NAME)
    }
    fn spec(&self) -> ToolSpec {
        create_kb_prohibit_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{KB_PROHIBIT_TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: ProhibitArgs = parse_arguments(arguments)?;
            let cwd = invocation.turn.cwd.as_path().to_path_buf();
            let root = codex_inocode_knowledge::kb_root(&cwd);
            let value = json!({
                "title": args.title, "quote": args.quote, "reason": args.reason,
                "strength": args.strength, "match": args.match_spec,
            });
            let thread = invocation.session.thread_id.to_string();
            let result = codex_inocode_knowledge::kb::prohibit(&root, &value, &thread);
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                serde_json::to_string_pretty(&result).unwrap_or_default(),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for KbProhibitHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_asks_for_the_users_words_and_a_match() {
        let ToolSpec::Function(tool) = create_kb_prohibit_tool() else {
            panic!("function tool expected");
        };
        assert_eq!(tool.name, "kb_prohibit");
        assert!(tool.description.contains("quoting their words"));
        assert!(tool.description.contains("Give `match`"));
    }
}
