//! Model-facing, read-only tools of the knowledge database (`knowledge.enabled`):
//! `search_knowledge` and `read_knowledge`.
//!
//! The database (`ext/inocode-knowledge`) holds the project's rules (things not to do), failure
//! cases, decisions, facts, issues, contracts and procedures. The first turn already carries the
//! list of blocking rules; these tools are how the model looks something up when it needs it.
//! There is deliberately no write tool: rules are written by the user.

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_inocode_knowledge::Kind;
use codex_inocode_knowledge::ListQuery;
use codex_inocode_knowledge::Record;
use codex_inocode_knowledge::Status;
use codex_inocode_knowledge::Store;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const SEARCH_KNOWLEDGE_TOOL_NAME: &str = "search_knowledge";
pub(crate) const READ_KNOWLEDGE_TOOL_NAME: &str = "read_knowledge";

fn text_prop(description: &str) -> JsonSchema {
    JsonSchema::string(Some(description.to_string()))
}

fn tool(
    name: &str,
    description: &str,
    properties: BTreeMap<String, JsonSchema>,
    required: &[&str],
) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(required.iter().map(|r| (*r).to_string()).collect()),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn create_search_knowledge_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            text_prop("Words to look for in the titles and texts (case-insensitive, all words must match). Omit to list the most recent."),
        ),
        (
            "kind".to_string(),
            text_prop("Only this kind: `rule` (things not to do), `failure_case`, `issue`, `decision`, `fact`, `contract` or `procedure`."),
        ),
        (
            "include_inactive".to_string(),
            JsonSchema::boolean(Some(
                "Also list records that were superseded or retracted (default false).".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of results (default 20).".to_string())),
        ),
    ]);
    tool(
        SEARCH_KNOWLEDGE_TOOL_NAME,
        "Search this project's knowledge database: the rules about what must not be done, past \
failure cases and what worked, decisions, known facts, open issues, contracts and procedures. \
Check it before an action that could be forbidden or that may have gone wrong before, for example \
before deleting or force-pushing, touching a protected file, or retrying something that failed. \
The first turn only lists the blocking rules; this finds the rest. Each result has the id, kind, \
status, title and a short summary; use `read_knowledge` with the id for the full text. It only \
reads: it cannot add or change rules.",
        properties,
        &[],
    )
}

pub(crate) fn create_read_knowledge_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "id".to_string(),
        text_prop("A record id returned by `search_knowledge`, for example `rule-0003`."),
    )]);
    tool(
        READ_KNOWLEDGE_TOOL_NAME,
        "Read one knowledge record in full: its text, reason, what to avoid, what worked, where it \
came from and how it is enforced. Use it on an id from `search_knowledge` when the summary is not \
enough to decide what to do.",
        properties,
        &["id"],
    )
}

fn model_error(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

fn out(value: serde_json::Value) -> Box<dyn crate::tools::context::ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
        Some(true),
    ))
}

fn as_str<T: serde::Serialize>(value: &T) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    include_inactive: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

fn search(store: &Store, args: SearchArgs) -> Result<serde_json::Value, String> {
    let kind = match args.kind.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => Some(Kind::parse(k).ok_or_else(|| {
            let all: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
            format!("kind must be one of {}", all.join(", "))
        })?),
        None => None,
    };
    let include_inactive = args.include_inactive.unwrap_or(false);
    let limit = args.limit.unwrap_or(20).clamp(1, 100);
    // Fetch everything that matches, then drop what does not apply here (a rule narrowed to
    // another folder) and what is no longer in force.
    let (rows, _) = store.list(&ListQuery {
        kind,
        query: args.query,
        limit: 0,
        ..Default::default()
    });
    let results: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            (include_inactive || row.record.status.is_live())
                && (row.record.kind != Kind::Rule || store.applies(&row.record, &[]))
        })
        .take(limit)
        .map(|row| {
            let r = row.record;
            json!({
                "id": r.id,
                "kind": r.kind.as_str(),
                "scope": as_str(&r.scope),
                "status": as_str(&r.status),
                "title": r.title,
                "severity": r.severity.as_ref().and_then(as_str),
                "summary": row.summary,
                "doNot": r.do_not,
                "whatWorked": r.what_worked,
            })
        })
        .collect();
    Ok(json!({ "count": results.len(), "results": results }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    id: String,
}

fn full(r: &Record) -> serde_json::Value {
    let mut v = serde_json::to_value(r).unwrap_or_default();
    if let Some(m) = v.as_object_mut() {
        // Bookkeeping the model has no use for.
        for key in ["createdBy", "createdAt", "enforcedBy"] {
            m.remove(key);
        }
        m.insert("kind".to_string(), json!(r.kind.as_str()));
    }
    v
}

fn read(store: &Store, args: ReadArgs) -> Result<serde_json::Value, String> {
    let id = args.id.trim();
    let found: Vec<serde_json::Value> = store
        .all(None)
        .iter()
        .filter(|r| r.id == id)
        .map(full)
        .collect();
    if found.is_empty() {
        return Err(format!("no knowledge record with id {id}; find ids with search_knowledge"));
    }
    Ok(json!({ "records": found }))
}

fn store_for(invocation: &ToolInvocation) -> Store {
    codex_inocode_knowledge::store_for(invocation.turn.cwd.as_path())
}

fn arguments(invocation: &ToolInvocation, tool: &str) -> Result<String, FunctionCallError> {
    match &invocation.payload {
        ToolPayload::Function { arguments } => Ok(arguments.clone()),
        _ => Err(model_error(format!("{tool} handler received unsupported payload"))),
    }
}

pub struct SearchKnowledgeHandler;
pub struct ReadKnowledgeHandler;

impl ToolExecutor<ToolInvocation> for SearchKnowledgeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SEARCH_KNOWLEDGE_TOOL_NAME)
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        create_search_knowledge_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = arguments(&invocation, SEARCH_KNOWLEDGE_TOOL_NAME)?;
            let args: SearchArgs = parse_arguments(&arguments)?;
            search(&store_for(&invocation), args).map(out).map_err(model_error)
        })
    }
}

impl ToolExecutor<ToolInvocation> for ReadKnowledgeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_KNOWLEDGE_TOOL_NAME)
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        create_read_knowledge_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = arguments(&invocation, READ_KNOWLEDGE_TOOL_NAME)?;
            let args: ReadArgs = parse_arguments(&arguments)?;
            read(&store_for(&invocation), args).map(out).map_err(model_error)
        })
    }
}

impl CoreToolRuntime for SearchKnowledgeHandler {}
impl CoreToolRuntime for ReadKnowledgeHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_inocode_knowledge::Severity;

    fn name_and_description(spec: ToolSpec) -> (String, String) {
        let ToolSpec::Function(tool) = spec else {
            panic!("function tool expected");
        };
        (tool.name, tool.description)
    }

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(Some(d.path().join("proj")), d.path().join("home"));
        (d, s)
    }

    #[test]
    fn descriptions_say_when_to_call_and_that_it_only_reads() {
        let (name, d) = name_and_description(create_search_knowledge_tool());
        assert_eq!(name, "search_knowledge");
        assert!(d.contains("Check it before"));
        assert!(d.contains("cannot add or change"));
        let (name, d) = name_and_description(create_read_knowledge_tool());
        assert_eq!(name, "read_knowledge");
        assert!(d.contains("search_knowledge"));
    }

    #[test]
    fn search_finds_rules_failures_and_hides_inactive() {
        let (_d, s) = store();
        let mut r = Record::new(Kind::Rule, "force push は禁止");
        r.severity = Some(Severity::Block);
        r.reason = Some("履歴が消える".into());
        let rid = s.write(r).unwrap();
        s.write(Record::new(Kind::Fact, "ビルドは Windows だけ")).unwrap();
        let mut old = Record::new(Kind::Rule, "古い禁止事項 force");
        old.status = Status::Retracted;
        s.write(old).unwrap();

        let found = search(&s, SearchArgs { query: Some("force".into()), kind: None, include_inactive: None, limit: None }).unwrap();
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["id"], rid.as_str());
        assert_eq!(found["results"][0]["severity"], "block");
        assert_eq!(found["results"][0]["summary"], "履歴が消える");

        let all = search(&s, SearchArgs { query: Some("force".into()), kind: None, include_inactive: Some(true), limit: None }).unwrap();
        assert_eq!(all["count"], 2);

        let rules = search(&s, SearchArgs { query: None, kind: Some("rule".into()), include_inactive: None, limit: None }).unwrap();
        assert_eq!(rules["count"], 1);
        let none = search(&s, SearchArgs { query: Some("存在しない語".into()), kind: None, include_inactive: None, limit: None }).unwrap();
        assert_eq!(none["count"], 0);
        assert!(search(&s, SearchArgs { query: None, kind: Some("nope".into()), include_inactive: None, limit: None }).is_err());
    }

    #[test]
    fn search_hides_a_rule_narrowed_to_another_folder() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let ws = d.path().join("ws");
        let gui = ws.join("gui");
        let cli = ws.join("cli");
        std::fs::create_dir_all(&gui).unwrap();
        std::fs::create_dir_all(&cli).unwrap();
        let mut r = Record::new(Kind::Rule, "GUI だけの規則");
        r.applies_to = vec!["gui".into()];
        Store::new(Some(ws.clone()), home.clone()).write(r).unwrap();
        let at = |cwd: &std::path::Path| {
            Store::new(Some(ws.clone()), home.clone()).with_session_cwd(cwd.to_path_buf())
        };
        let q = || SearchArgs { query: None, kind: None, include_inactive: None, limit: None };
        assert_eq!(search(&at(&gui), q()).unwrap()["count"], 1);
        assert_eq!(search(&at(&cli), q()).unwrap()["count"], 0);
    }

    #[test]
    fn read_returns_the_full_record_or_a_helpful_error() {
        let (_d, s) = store();
        let mut f = Record::new(Kind::FailureCase, "ロック失敗");
        f.symptom = Some("database is locked".into());
        f.do_not = Some("リトライを無限に回さない".into());
        f.body = "詳細な経緯".into();
        let id = s.write(f).unwrap();
        let got = read(&s, ReadArgs { id: id.clone() }).unwrap();
        let rec = &got["records"][0];
        assert_eq!(rec["kind"], "failure_case");
        assert_eq!(rec["doNot"], "リトライを無限に回さない");
        assert_eq!(rec["body"], "詳細な経緯");
        assert!(rec.get("createdBy").is_none());
        let err = read(&s, ReadArgs { id: "rule-9999".into() }).unwrap_err();
        assert!(err.contains("search_knowledge"));
    }

    #[test]
    fn arguments_reject_unknown_fields() {
        assert!(serde_json::from_str::<SearchArgs>(r#"{"query":"a","x":1}"#).is_err());
        assert!(serde_json::from_str::<ReadArgs>(r#"{"id":"rule-0001"}"#).is_ok());
    }
}
