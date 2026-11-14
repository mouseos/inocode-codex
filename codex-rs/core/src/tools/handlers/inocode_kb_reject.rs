//! `kb_reject`: the user says in the chat that a record is wrong, and the model takes it out for
//! them (the same as the reject button on the screen: `inocode/kb/reject`).

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

pub(crate) const KB_REJECT_TOOL_NAME: &str = "kb_reject";

pub(crate) fn create_kb_reject_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "kind".to_string(),
            JsonSchema::string(Some("`page` (a page or a fact), `evidence` (one recorded quote), `failure` or `prohibition`.".to_string())),
        ),
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "The page title, or the id of the failure record or prohibition (from `search_knowledge` or the result of the call that added it)."
                    .to_string(),
            )),
        ),
        (
            "claim".to_string(),
            JsonSchema::string(Some("For `evidence`: the claim as it was recorded (only that quote is taken off the page).".to_string())),
        ),
        (
            "source_ref".to_string(),
            JsonSchema::string(Some("For `evidence`: the `sourceRef` the record returned.".to_string())),
        ),
        (
            "reason".to_string(),
            JsonSchema::string(Some("Why, in the user's words, if they gave one. Optional.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: KB_REJECT_TOOL_NAME.to_string(),
        description: "Take a record out of this project's knowledge when the user says it is wrong \
(only on the user's word, never on your own). A page goes back to its version before your last \
change, or is removed if you made it; a failure record is retracted. It is not recorded again. The \
result says what was done."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["kind".to_string(), "id".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectArgs {
    kind: String,
    id: String,
    #[serde(default)]
    claim: Option<String>,
    #[serde(default)]
    source_ref: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

pub struct KbRejectHandler;

impl ToolExecutor<ToolInvocation> for KbRejectHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(KB_REJECT_TOOL_NAME)
    }
    fn spec(&self) -> ToolSpec {
        create_kb_reject_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{KB_REJECT_TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: RejectArgs = parse_arguments(arguments)?;
            let cwd = invocation.turn.cwd.as_path().to_path_buf();
            let mut params = json!({
                "cwd": cwd.to_string_lossy(), "kind": args.kind, "id": args.id,
                "reason": args.reason, "by": "user (in the chat)",
            });
            if let (Some(c), Some(r)) = (args.claim, args.source_ref) {
                params["claim"] = json!(c);
                params["sourceRef"] = json!(r);
            }
            let text = match codex_inocode_knowledge::kb::call("reject", &params) {
                Ok(v) => {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                }
                Err(e) => return Err(FunctionCallError::RespondToModel(e.message)),
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(text, Some(true))))
        })
    }
}

impl CoreToolRuntime for KbRejectHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_says_it_is_only_on_the_users_word() {
        let ToolSpec::Function(tool) = create_kb_reject_tool() else {
            panic!("function tool expected");
        };
        assert_eq!(tool.name, "kb_reject");
        assert!(tool.description.contains("only on the user's word"));
    }
}
