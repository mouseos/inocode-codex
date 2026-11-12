//! Responses API tool definitions for persisted thread goals.

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const GET_GOAL_TOOL_NAME: &str = "get_goal";
pub const CREATE_GOAL_TOOL_NAME: &str = "create_goal";
pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";
pub const REQUEST_GOAL_EDIT_TOOL_NAME: &str = "request_goal_edit";

pub fn create_get_goal_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: GET_GOAL_TOOL_NAME.to_string(),
        description: "Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
        output_schema: None,
    })
}

pub fn create_create_goal_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "objective".to_string(),
            JsonSchema::string(Some(
                "Required. The concrete objective to start pursuing. This starts a new active goal when no goal exists."
                    .to_string(),
            )),
        ),
        (
            "token_budget".to_string(),
            JsonSchema::integer(Some(
                "Positive token budget for the new goal. Omit unless explicitly requested."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: CREATE_GOAL_TOOL_NAME.to_string(),
        description: format!(
            r#"Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks.
Set token_budget only when an explicit token budget is requested. Fails if any goal exists, finished or not: a goal is replaced or reworded only by the user, or by the user's approval of {REQUEST_GOAL_EDIT_TOOL_NAME}. Use {UPDATE_GOAL_TOOL_NAME} only for status."#
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["objective".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_update_goal_tool() -> ToolSpec {
    let evidence_item = JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string(Some(
                    "`file` (a file you looked at), `command` (a command you ran), `output` or `other`."
                        .to_string(),
                )),
            ),
            (
                "detail".to_string(),
                JsonSchema::string(Some(
                    "The path, the exact command line, or the observed text.".to_string(),
                )),
            ),
            (
                "exit_code".to_string(),
                JsonSchema::integer(Some("Exit code, for a command.".to_string())),
            ),
        ]),
        Some(vec!["detail".to_string()]),
        Some(false.into()),
    );
    let requirement = JsonSchema::object(
        BTreeMap::from([
            (
                "requirement".to_string(),
                JsonSchema::string(Some("One requirement of the objective.".to_string())),
            ),
            (
                "claim".to_string(),
                JsonSchema::string(Some(
                    "Your own claim about it, for example `met`, `partly met`, `not met`."
                        .to_string(),
                )),
            ),
            (
                "evidence".to_string(),
                JsonSchema::array(evidence_item, Some("What shows it.".to_string())),
            ),
        ]),
        Some(vec!["requirement".to_string()]),
        Some(false.into()),
    );
    let properties = BTreeMap::from([
        (
        "requirements".to_string(),
        JsonSchema::array(
            requirement,
            Some(
                "Optional, for `complete`: the requirements you derived from the objective, each with your claim and the evidence behind it (files you looked at, commands you ran with their exit codes). The host records this and shows it to the user. It does not block completion."
                    .to_string(),
            ),
        ),
        ),
        (
        "status".to_string(),
        JsonSchema::string_enum(
            vec![json!("complete"), json!("blocked"), json!("paused")],
            Some(
                "Required. `paused` requires an explicit user request. Set to `complete` only when the objective is achieved and no required work remains. Set to `blocked` only after the same blocking condition has recurred for at least three consecutive goal turns and the agent is at an impasse. After a previously blocked goal is resumed, the resumed run starts a fresh blocked audit."
                    .to_string(),
            ),
        ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: UPDATE_GOAL_TOOL_NAME.to_string(),
        description: r#"Update the existing goal.
Set status to `paused` only at the user's explicit request to pause this goal, never on your own initiative. It is refused during an automatic continuation turn. Ask if unclear; a later resume revokes that request. Report the returned status and stop goal work. Budget limits take precedence over pausing.
Set status to `complete` only when the objective has actually been achieved and no required work remains.
Set status to `blocked` only when the same blocking condition has repeated for at least three consecutive goal turns, counting the original/user-triggered turn and any automatic continuations, and the agent cannot make meaningful progress without user input or an external-state change.
If the user resumes a goal that was previously marked `blocked`, treat the resumed run as a fresh blocked audit. If the same blocking condition then repeats for at least three consecutive resumed goal turns, set status to `blocked` again.
Once the blocked threshold is satisfied, do not keep reporting that you are still blocked while leaving the goal active; set status to `blocked`.
Requirements that only a person can satisfy (their own action, approval, permission or credentials, or a check on their real device) count as that kind of blocker once everything you can do is done; a turn that only waits for or asks the person again counts toward the three turns.
Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification.
Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work.
You cannot use this tool to resume, budget-limit, or usage-limit a goal; those status changes are controlled by the user or system.
When marking a budgeted goal achieved with status `complete`, report the final token usage from the tool result to the user."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["status".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_request_goal_edit_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "objective".to_string(),
            JsonSchema::string(Some(
                "Required. The full new objective text you propose.".to_string(),
            )),
        ),
        (
            "reason".to_string(),
            JsonSchema::string(Some(
                "Required. Why the objective should change. The user sees this as your claim."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: REQUEST_GOAL_EDIT_TOOL_NAME.to_string(),
        description: r#"Ask the user to change the goal's objective. This changes nothing by itself: the user sees the current and the proposed text side by side and approves or rejects it. Use it only when the objective itself has to change; never to narrow the objective so that it can be completed. If it is approved the objective is replaced; keep working on the current objective until then."#
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["objective".to_string(), "reason".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
