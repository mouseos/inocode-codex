//! Model-facing tools for the project's source pages (`evidence.enabled`):
//! `record_evidence`, `search_evidence` and `verify_evidence_document`.
//!
//! They write and read the same data the GUI's mind map and evidence tab read (`inocode/kb/page/*`):
//! the original text of each source is kept unchanged in the project, and each fact goes onto a
//! page as a line plus a verbatim quote block (`> quote:<source ref>: ...`). What the model writes is
//! registered at once and marked as a draft until the user confirms it (decision Q-12).

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
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

pub(crate) const RECORD_EVIDENCE_TOOL_NAME: &str = "record_evidence";
pub(crate) const SEARCH_EVIDENCE_TOOL_NAME: &str = "search_evidence";
pub(crate) const VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME: &str = "verify_evidence_document";

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

pub(crate) fn create_record_evidence_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "claim".to_string(),
            text_prop("The fact you found, as one self-contained sentence. For a price include the item, the amount, the currency and the date or condition it applies to."),
        ),
        (
            "quote".to_string(),
            text_prop("The exact words of the source that support the claim, copied verbatim, on one line (a price line, a sentence of an article). It must appear unchanged in source_text when that is given."),
        ),
        (
            "source_url".to_string(),
            text_prop("Where you got it: the URL of the page, or a file path. Omit only when there is none."),
        ),
        (
            "source_title".to_string(),
            text_prop("Title of the page or document. Defaults to the URL."),
        ),
        (
            "source_text".to_string(),
            text_prop("Optional. The text of the page or document you read, to keep as the original. When omitted, the quote alone is kept as the original."),
        ),
        (
            "page".to_string(),
            text_prop("Optional. The title of the page to add it to, for example the topic you are researching. Defaults to the source title. The page is created when it does not exist."),
        ),
    ]);
    tool(
        RECORD_EVIDENCE_TOOL_NAME,
        "Record a fact you found, with its source, on a page of this project's notes. Use it \
whenever you look something up on the web or in a document and the result matters later, and \
whenever the user asks you to keep something to read again later: a price, a specification, a \
date, a figure, a statement. Give the claim, the verbatim quote that supports it and where it came \
from (URL or file); the retrieval date is added automatically. The original text is kept, the fact \
and its quote are added to the page (created if needed), and the page is registered at once, \
marked as a draft until the user confirms it. It is shown in the GUI's evidence tab and mind map \
and found again with `search_evidence`. Returns the page and the source reference.",
        properties,
        &["claim", "quote"],
    )
}

pub(crate) fn create_search_evidence_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            text_prop("Words to look for in the pages (case-insensitive, all must appear). Omit to list the most recent."),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of pages (default 20).".to_string())),
        ),
    ]);
    tool(
        SEARCH_EVIDENCE_TOOL_NAME,
        "Search this project's notes (pages written with `record_evidence` or `kb_propose`, \
including in other sessions, and pages the user wrote). Check it before looking something up \
again, for example a price you may already have recorded. Each result has the page, whether it is \
still a draft, and its quotes with the source (title, URL, retrieval date) so you can say where it \
came from and how old it is.",
        properties,
        &[],
    )
}

pub(crate) fn create_verify_evidence_document_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "text".to_string(),
            text_prop("The document or answer to check."),
        ),
        (
            "source_refs".to_string(),
            JsonSchema::array(
                text_prop("A source reference returned by `record_evidence` or `search_evidence`."),
                Some("The sources the document must rest on. Omit to use every source of this project.".to_string()),
            ),
        ),
    ]);
    tool(
        VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME,
        "Check a document you wrote against the recorded sources. It reports, and does not change \
anything: quotes that do not appear word for word in the sources, sentences without a citation \
marker, and terms (amounts, dates, names of offences or articles) that appear in the document but \
in none of the sources. Use it on a report or an answer that states facts taken from sources, \
before you hand it over.",
        properties,
        &["text"],
    )
}

fn model_error(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

/// The arguments, the project folder of the turn, the thread id and the call id.
fn arguments(
    invocation: ToolInvocation,
    tool: &str,
) -> Result<(String, PathBuf, String, String), FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(model_error(format!("{tool} handler received unsupported payload")));
    };
    let cwd = turn.cwd.as_path().to_path_buf();
    let root = project_root(&cwd);
    Ok((arguments, root, session.thread_id.to_string(), call_id))
}

fn project_root(cwd: &Path) -> PathBuf {
    codex_inocode_knowledge::kb_root(cwd)
}

fn out(value: serde_json::Value) -> Box<dyn crate::tools::context::ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
        Some(true),
    ))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    claim: String,
    quote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<String>,
}

fn record(root: &Path, args: &RecordArgs, created_by: &str) -> serde_json::Value {
    let value = serde_json::to_value(args).unwrap_or_default();
    codex_inocode_knowledge::kb::record_evidence(root, &value, created_by)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn search(root: &Path, args: SearchArgs) -> serde_json::Value {
    codex_inocode_knowledge::kb::search_evidence(
        root,
        args.query.as_deref(),
        args.limit.unwrap_or(20),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    text: String,
    #[serde(default)]
    source_refs: Option<Vec<String>>,
}

fn verify(root: &Path, args: VerifyArgs) -> Result<serde_json::Value, String> {
    let refs = args.source_refs.unwrap_or_default();
    let mut report = codex_inocode_knowledge::kb::verify_document(root, &args.text, &refs)
        .map_err(|e| e.message)?;
    report["note"] = json!("Findings are reports only; nothing was changed or blocked.");
    Ok(report)
}

pub struct RecordEvidenceHandler;
pub struct SearchEvidenceHandler;
pub struct VerifyEvidenceDocumentHandler;

impl ToolExecutor<ToolInvocation> for RecordEvidenceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(RECORD_EVIDENCE_TOOL_NAME)
    }
    fn spec(&self) -> ToolSpec {
        create_record_evidence_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let (arguments, root, thread_id, call_id) = arguments(invocation, RECORD_EVIDENCE_TOOL_NAME)?;
            let args: RecordArgs = parse_arguments(&arguments)?;
            // A refusal is a result too (status denied, with the reason), so the card can show it
            // and the model can fix the call.
            let result = record(&root, &args, &format!("model:{thread_id}"));
            codex_inocode_knowledge::notify_tool_used(&thread_id, &call_id, RECORD_EVIDENCE_TOOL_NAME,
                &format!("根拠を記録しました: {}", args.claim), &result);
            Ok(out(result))
        })
    }
}

impl ToolExecutor<ToolInvocation> for SearchEvidenceHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(SEARCH_EVIDENCE_TOOL_NAME)
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        create_search_evidence_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let (arguments, root, thread_id, call_id) = arguments(invocation, SEARCH_EVIDENCE_TOOL_NAME)?;
            let args: SearchArgs = parse_arguments(&arguments)?;
            let query = args.query.clone().unwrap_or_default();
            let result = search(&root, args);
            let n = result.get("count").and_then(serde_json::Value::as_u64).unwrap_or(0);
            codex_inocode_knowledge::notify_tool_used(&thread_id, &call_id, SEARCH_EVIDENCE_TOOL_NAME,
                &if query.trim().is_empty() { format!("根拠を検索しました — {n} 件") } else { format!("根拠を検索しました: {} — {n} 件", query.trim()) }, &result);
            Ok(out(result))
        })
    }
}

impl ToolExecutor<ToolInvocation> for VerifyEvidenceDocumentHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME)
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        create_verify_evidence_document_tool()
    }
    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let (arguments, root, thread_id, call_id) = arguments(invocation, VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME)?;
            let args: VerifyArgs = parse_arguments(&arguments)?;
            let result = verify(&root, args).map_err(model_error)?;
            let n = result.get("findings").and_then(serde_json::Value::as_array).map_or(0, Vec::len);
            codex_inocode_knowledge::notify_tool_used(&thread_id, &call_id, VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME,
                &format!("文書を根拠と照合しました — 指摘 {n} 件"), &serde_json::Value::Null);
            Ok(out(result))
        })
    }
}

impl CoreToolRuntime for RecordEvidenceHandler {}
impl CoreToolRuntime for SearchEvidenceHandler {}
impl CoreToolRuntime for VerifyEvidenceDocumentHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_and_description(spec: ToolSpec) -> (String, String) {
        let ToolSpec::Function(tool) = spec else {
            panic!("function tool expected");
        };
        (tool.name, tool.description)
    }

    #[test]
    fn descriptions_say_when_to_call() {
        let (name, d) = name_and_description(create_record_evidence_tool());
        assert_eq!(name, "record_evidence");
        assert!(d.contains("price") && d.contains("source"));
        assert!(d.contains("read again later") && d.contains("draft"));
        let (name, d) = name_and_description(create_search_evidence_tool());
        assert_eq!(name, "search_evidence");
        assert!(d.contains("Check it before looking something up again"));
        let (name, _) = name_and_description(create_verify_evidence_document_tool());
        assert_eq!(name, "verify_evidence_document");
    }

    #[test]
    fn arguments_reject_unknown_fields() {
        assert!(serde_json::from_str::<RecordArgs>(r#"{"claim":"a","quote":"b","x":1}"#).is_err());
        assert!(
            serde_json::from_str::<RecordArgs>(r#"{"claim":"a","quote":"b","page":"p"}"#).is_ok()
        );
        assert!(serde_json::from_str::<VerifyArgs>(r#"{"text":"a","source_ids":["x"]}"#).is_err());
    }
}
