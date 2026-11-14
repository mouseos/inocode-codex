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
        "Search this project's knowledge database: the overview, the pages (write-ups, notes and \
researched facts with their sources, ids `page:<title>`), the prohibitions, the checklists, the \
rules about what must not be done, past failure cases and what worked, decisions, known facts, open \
issues, contracts and procedures. Check it before starting a task, so you build on what is already \
known instead of looking it up again. \
Check it before an action that could be forbidden or that may have gone wrong before, for example \
before deleting or force-pushing, touching a protected file, or retrying something that failed. \
The first turn only lists the blocking rules; this finds the rest. Each result has the id, kind, \
status, title and a short summary; use `read_knowledge` with the id for the full text. A failure \
case is not a ban: it says under which conditions something failed (`whenItFailed`). Compare those \
conditions with the situation now and avoid the action only when they match; when they do not \
match, it may be used. A case marked `stateDependent` has also succeeded under other conditions \
(`worksWhen`). When the conditions are unknown (it failed once without the state being checked), \
do a cheap read-only check of the state first if there is one, then try once; if you do not know \
how to check, just try. If the same result keeps coming in the same state, think of another way. \
It only reads: it cannot add or change rules.",
        properties,
        &[],
    )
}

pub(crate) fn create_read_knowledge_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "id".to_string(),
        text_prop("An id returned by `search_knowledge`, for example `rule-0003`, `page:<title>`, `prohibition:<id>`, `checklist:<role>` or `overview`."),
    )]);
    tool(
        READ_KNOWLEDGE_TOOL_NAME,
        "Read one knowledge record in full: its text, reason, what to avoid, what worked, where it \
came from and how it is enforced. For a failure case it gives the conditions it failed under \
(`whenItFailed`); avoid the action only if the situation now matches them. Use it on an id from \
`search_knowledge` when the summary is not enough to decide what to do.",
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

/// A failure case that records no failure: a call that succeeded, or a stall made only of reading
/// and searching. Kept in the database, never returned as a failure.
fn not_a_failure(r: &Record) -> bool {
    if r.kind != Kind::FailureCase {
        return false;
    }
    let symptom = r.symptom.as_deref().unwrap_or("").trim();
    // The advisor's automatic records are candidates (doc 60): kept, not offered to the model.
    codex_inocode_knowledge::kb::is_candidate(r)
        || symptom.ends_with("-> ok")
        || symptom.starts_with("[built-in search output]")
}

/// "Under these conditions it failed ..." with the conditions first. Not a prohibition.
/// An old record with none of the detail fields says so.
fn when_it_failed(r: &Record) -> Option<String> {
    if r.kind != Kind::FailureCase {
        return None;
    }
    let has = |v: &Option<String>| v.as_deref().is_some_and(|x| !x.trim().is_empty());
    let no_detail = !(has(&r.project)
        || has(&r.purpose)
        || has(&r.attempted)
        || has(&r.outcome)
        || has(&r.conditions)
        || has(&r.cause)
        || !r.unknown_conditions.is_empty());
    let mut out = String::new();
    if no_detail {
        out.push_str("No detail (recorded automatically). ");
    }
    let unknown = r.conditions_known == Some(false) || !has(&r.conditions);
    if has(&r.conditions) && r.conditions_known != Some(false) {
        out.push_str(&format!("When: {}. ", r.conditions.as_deref().unwrap_or("").trim()));
    }
    if let Some(s) = r.symptom.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("It failed with: {s}. "));
    }
    if let Some(c) = r.cause.as_deref().filter(|c| !c.trim().is_empty()) {
        out.push_str(&format!(
            "Cause (a hypothesis, {}): {c}. ",
            r.cause_confidence.as_deref().unwrap_or("not confirmed")
        ));
    }
    if r.state_dependent == Some(true) {
        out.push_str("The same call has also succeeded");
        match r.works_when.as_deref().filter(|w| !w.trim().is_empty()) {
            Some(w) => out.push_str(&format!(" when: {w}. ")),
            None => out.push_str(" under other conditions. "),
        }
    } else if unknown {
        out.push_str("It failed once without the state being checked");
        if r.unknown_conditions.is_empty() {
            out.push_str(". ");
        } else {
            out.push_str(&format!(" (not known: {}). ", r.unknown_conditions.join(", ")));
        }
        if r.unknown_conditions.is_empty() {
            out.push_str("Try it; if a cheap read-only check of the state is at hand, do it first. ");
        } else {
            out.push_str("A cheap read-only check of those first, then one try. ");
        }
    }
    out.push_str("This is not a ban: avoid it only if the situation now matches these conditions.");
    Some(out)
}

/// Only the first few failure cases of a result carry the full "when it failed" text.
const DETAILED_FAILURES: usize = 3;

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
                && !not_a_failure(&row.record)
                && (row.record.kind != Kind::Rule || store.applies(&row.record, &[]))
        })
        .take(limit)
        .scan(0usize, |failures, row| {
            let detailed = row.record.kind != Kind::FailureCase || {
                *failures += 1;
                *failures <= DETAILED_FAILURES
            };
            Some((row, detailed))
        })
        .map(|(row, detailed)| {
            let r = row.record;
            json!({
                "id": r.id,
                "kind": r.kind.as_str(),
                "scope": as_str(&r.scope),
                "status": as_str(&r.status),
                "title": r.title,
                "severity": r.severity.as_ref().and_then(as_str),
                "summary": row.summary,
                "whenItFailed": if detailed { when_it_failed(&r) } else { None },
                "stateDependent": r.state_dependent,
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
        if let Some(text) = when_it_failed(r) {
            m.insert("whenItFailed".to_string(), json!(text));
        }
        if not_a_failure(r) {
            m.insert(
                "note".to_string(),
                json!("This record is not a failure (the call succeeded, or it only read and searched)."),
            );
        }
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

/// The knowledge of the project the session is in (not of a folder above it), and the common one.
fn store_for(invocation: &ToolInvocation) -> Store {
    let cwd = invocation.turn.cwd.as_path();
    Store::new(Some(codex_inocode_knowledge::kb_root(cwd)), codex_inocode_knowledge::inocode_home())
        .with_session_cwd(cwd.to_path_buf())
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
            let store = store_for(&invocation);
            let query = args.query.clone();
            let limit = args.limit.unwrap_or(20).clamp(1, 100);
            let only_legacy_kind = args.kind.as_deref().is_some_and(|k| !k.is_empty());
            let mut value = search(&store, args).map_err(model_error)?;
            // The redesigned knowledge of the project (overview, pages, prohibitions, checklists).
            if !only_legacy_kind && let Some(root) = store.project_root() {
                let extra = codex_inocode_knowledge::kb::search_project_knowledge(root, query.as_deref(), limit);
                if let Some(results) = value.get_mut("results").and_then(|r| r.as_array_mut()) {
                    results.extend(extra);
                    results.truncate(limit);
                    let n = results.len();
                    value["count"] = json!(n);
                }
            }
            Ok(out(value))
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
            let store = store_for(&invocation);
            if let Some(root) = store.project_root()
                && let Some(found) = codex_inocode_knowledge::kb::read_project_knowledge(root, args.id.trim())
            {
                return found.map(out).map_err(model_error);
            }
            read(&store, args).map(out).map_err(model_error)
        })
    }
}

impl CoreToolRuntime for SearchKnowledgeHandler {}
impl CoreToolRuntime for ReadKnowledgeHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_inocode_knowledge::Status;
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
    fn failure_cases_come_back_with_conditions_and_not_as_a_ban() {
        let (_d, s) = store();
        let mut f = Record::new(Kind::FailureCase, "失敗: adb shell");
        f.symptom = Some("adb: device offline".into());
        f.conditions = Some("fastboot の最中だった(直前: fastboot flash boot)".into());
        f.state_dependent = Some(true);
        f.works_when = Some("通常起動のあと".into());
        s.write(f).unwrap();
        let mut ok = Record::new(Kind::FailureCase, "失敗: list_events {}");
        ok.symptom = Some("list_events {} -> ok".into());
        s.write(ok).unwrap();
        let mut search_only = Record::new(Kind::FailureCase, "失敗: 8 tool calls");
        search_only.symptom = Some("[built-in search output]".into());
        s.write(search_only).unwrap();
        let q = |q: &str| SearchArgs { query: Some(q.into()), kind: None, include_inactive: None, limit: None };
        let found = search(&s, q("adb")).unwrap();
        assert_eq!(found["count"], 1);
        let text = found["results"][0]["whenItFailed"].as_str().unwrap();
        assert!(text.starts_with("When: fastboot の最中だった"));
        assert!(text.contains("通常起動のあと") && text.contains("not a ban"));
        assert_eq!(found["results"][0]["stateDependent"], true);
        // the advisor's automatic records are candidates, not returned
        let mut cand = Record::new(Kind::FailureCase, "失敗: cand");
        cand.symptom = Some("candidate boom".into());
        cand.created_by = Some("advisor".into());
        s.write(cand).unwrap();
        assert_eq!(search(&s, q("candidate")).unwrap()["count"], 0);
        // a success and a read-only stall are not returned as failures
        assert_eq!(search(&s, q("list_events")).unwrap()["count"], 0);
        assert_eq!(search(&s, q("tool calls")).unwrap()["count"], 0);
        let all = search(&s, SearchArgs { query: None, kind: Some("failure_case".into()), include_inactive: None, limit: None }).unwrap();
        assert_eq!(all["count"], 1);
        // a failure without recorded conditions says so
        let mut bare = Record::new(Kind::FailureCase, "失敗: x");
        bare.symptom = Some("boom".into());
        let id = s.write(bare).unwrap();
        let got = read(&s, ReadArgs { id }).unwrap();
        assert!(got["records"][0]["whenItFailed"].as_str().unwrap().contains("No detail"));
        // read by id still shows a success record, with a note
        let rid = s.all(None).iter().find(|r| r.title.contains("list_events")).unwrap().id.clone();
        let got = read(&s, ReadArgs { id: rid }).unwrap();
        assert!(got["records"][0]["note"].is_string());
    }

    #[test]
    fn unknown_conditions_and_old_records_are_said_plainly() {
        let (_d, s) = store();
        // An old automatic record: nothing but a signature and a symptom.
        let mut old = Record::new(Kind::FailureCase, "失敗: adb shell");
        old.symptom = Some("adb: device offline".into());
        s.write(old).unwrap();
        // A record that says what could not be checked, with a hypothesis for the cause.
        let mut unknown = Record::new(Kind::FailureCase, "失敗: flash boot");
        unknown.symptom = Some("flash failed".into());
        unknown.attempted = Some("fastboot flash boot".into());
        unknown.conditions_known = Some(false);
        unknown.unknown_conditions = vec!["端末の状態".into(), "直前の状態確認の有無".into()];
        unknown.cause = Some("ブートローダーが閉じていた".into());
        s.write(unknown).unwrap();
        let q = |q: &str| SearchArgs { query: Some(q.into()), kind: None, include_inactive: None, limit: None };
        let a = search(&s, q("adb")).unwrap();
        let text = a["results"][0]["whenItFailed"].as_str().unwrap();
        assert!(text.starts_with("No detail (recorded automatically)."), "{text}");
        assert!(text.contains("without the state being checked"));
        let f = search(&s, q("flash")).unwrap();
        let text = f["results"][0]["whenItFailed"].as_str().unwrap();
        assert!(!text.contains("No detail"));
        assert!(text.contains("端末の状態, 直前の状態確認の有無"), "{text}");
        assert!(text.contains("a hypothesis") && text.contains("not a ban"));
        // The tool description tells what to do when the conditions are unknown.
        let (_, d) = name_and_description(create_search_knowledge_tool());
        assert!(d.contains("cheap read-only check") && d.contains("just try"));
    }

    #[test]
    fn only_the_first_few_failures_carry_the_long_text() {
        let (_d, s) = store();
        for i in 0..5 {
            let mut f = Record::new(Kind::FailureCase, &format!("失敗: cmd{i}"));
            f.symptom = Some(format!("boom{i}"));
            s.write(f).unwrap();
        }
        let all = search(&s, SearchArgs { query: None, kind: Some("failure_case".into()), include_inactive: None, limit: None }).unwrap();
        let longs = all["results"].as_array().unwrap().iter().filter(|r| r["whenItFailed"].is_string()).count();
        assert_eq!(longs, 3);
        assert_eq!(all["count"], 5);
    }

    #[test]
    fn arguments_reject_unknown_fields() {
        assert!(serde_json::from_str::<SearchArgs>(r#"{"query":"a","x":1}"#).is_err());
        assert!(serde_json::from_str::<ReadArgs>(r#"{"id":"rule-0001"}"#).is_ok());
    }
}
