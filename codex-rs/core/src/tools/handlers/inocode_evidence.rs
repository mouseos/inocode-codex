//! Model-facing tools of the evidence database (`evidence.enabled`):
//! `record_evidence`, `search_evidence` and `verify_evidence_document`.
//!
//! The database itself (`ext/inocode-evidence`) is the same one the GUI's evidence tab reads:
//! sources (original text, kept unchanged), quotes (verbatim spans) and claims. These tools are
//! how the model puts what it found there, with its source, and gets it back later.

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_inocode_evidence::ClaimType;
use codex_inocode_evidence::Confidence;
use codex_inocode_evidence::NewClaim;
use codex_inocode_evidence::Store;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;

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
            text_prop("The exact words of the source that support the claim, copied verbatim (a price line, a sentence of an article). It must appear unchanged in source_text when that is given."),
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
            "claim_type".to_string(),
            text_prop("`fact` (default), `interpretation`, `inference` or `opinion`."),
        ),
        (
            "status".to_string(),
            text_prop("`reported` (default: the source says so), `verified` (you checked it against another source), `speculative`, `unverified`, `disputed` or `refuted`."),
        ),
        (
            "confidence".to_string(),
            text_prop("`high`, `medium` or `low`. Optional."),
        ),
    ]);
    tool(
        RECORD_EVIDENCE_TOOL_NAME,
        "Record a fact you found, together with its source, in the evidence database. Use it \
whenever you look something up on the web or in a document and the result matters later: a price, \
a specification, a date, a figure, a statement. Give the claim, the verbatim quote that supports \
it and where it came from (URL or file); the retrieval date is added automatically. The record is \
kept across sessions, shown in the GUI's evidence tab, and found again with `search_evidence`. \
Returns the ids of the source, the quote and the claim.",
        properties,
        &["claim", "quote"],
    )
}

pub(crate) fn create_search_evidence_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            text_prop("Words to look for in the recorded claims (case-insensitive). Omit to list the most recent."),
        ),
        (
            "status".to_string(),
            text_prop("Only claims with this status, for example `verified` or `disputed`."),
        ),
        (
            "limit".to_string(),
            JsonSchema::number(Some("Maximum number of results (default 20).".to_string())),
        ),
    ]);
    tool(
        SEARCH_EVIDENCE_TOOL_NAME,
        "Search the evidence database for facts recorded earlier with `record_evidence`, including \
in other sessions. Check it before looking something up again, for example a price you may \
already have recorded. Each result has the claim, its status, the supporting quote, and the \
source (title, URL, retrieval date) so you can say where it came from and how old it is.",
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
            "source_ids".to_string(),
            JsonSchema::array(
                text_prop("A source id returned by `record_evidence` or `search_evidence`."),
                Some("The sources the document must rest on. Omit to use every recorded source.".to_string()),
            ),
        ),
    ]);
    tool(
        VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME,
        "Check a document you wrote against recorded sources. It reports, and does not change \
anything: quotes that do not appear word for word in the sources, sentences without a citation \
marker, and terms (amounts, dates, names of offences or articles) that appear in the document but \
in none of the sources. Use it on a report or an answer that states facts taken from sources, \
before you hand it over.",
        properties,
        &["text"],
    )
}

fn store() -> Store {
    codex_inocode_evidence::default_store()
}

fn model_error(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

fn arguments(invocation: ToolInvocation, tool: &str) -> Result<(String, String), FunctionCallError> {
    let ToolInvocation {
        session, payload, ..
    } = invocation;
    let ToolPayload::Function { arguments } = payload else {
        return Err(model_error(format!("{tool} handler received unsupported payload")));
    };
    Ok((arguments, session.thread_id.to_string()))
}

fn out(value: serde_json::Value) -> Box<dyn crate::tools::context::ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
        Some(true),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordArgs {
    claim: String,
    quote: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    source_title: Option<String>,
    #[serde(default)]
    source_text: Option<String>,
    #[serde(default)]
    claim_type: Option<ClaimType>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    confidence: Option<Confidence>,
}

const STATUSES: &[&str] = &[
    "verified",
    "reported",
    "speculative",
    "unverified",
    "disputed",
    "refuted",
];

fn record(store: &Store, args: RecordArgs, created_by: &str) -> Result<serde_json::Value, String> {
    if args.claim.trim().is_empty() || args.quote.trim().is_empty() {
        return Err("claim and quote must not be empty".to_string());
    }
    let status = args.status.as_deref().unwrap_or("reported");
    if !STATUSES.contains(&status) {
        return Err(format!("status must be one of {}", STATUSES.join(", ")));
    }
    let url = args.source_url.as_deref().filter(|u| !u.trim().is_empty());
    let original = args
        .source_text
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&args.quote);
    if !original.contains(&args.quote) {
        return Err("the quote does not appear word for word in source_text; copy it exactly".to_string());
    }
    // The same page read again is one source.
    let existing = url.and_then(|url| {
        store.list_sources().into_iter().find(|s| {
            s.origin.as_deref() == Some(url)
                && store
                    .read_source(&s.id)
                    .is_some_and(|(_, text)| text == original)
        })
    });
    let source = match existing {
        Some(source) => source,
        None => store
            .import_bytes(
                args.source_title.as_deref().or(url).unwrap_or("note"),
                if url.is_some_and(|u| u.starts_with("http")) { "web" } else { "text" },
                original.as_bytes(),
                None,
                url,
            )
            .map_err(|e| format!("could not save the source: {e}"))?,
    };
    let span = store
        .add_quote(&source.id, &args.quote)
        .map_err(|e| format!("could not record the quote: {e}"))?;
    let claim = store
        .add_claim(NewClaim {
            text: &args.claim,
            claim_type: args.claim_type.unwrap_or(ClaimType::Fact),
            status,
            confidence: args.confidence,
            span_ids: vec![span.id.clone()],
            premise_ids: Vec::new(),
            created_by,
        })
        .map_err(|e| format!("could not record the claim: {e}"))?;
    Ok(json!({
        "claimId": claim.id,
        "quoteId": span.id,
        "sourceId": source.id,
        "sourceUrl": source.origin,
        "retrievedAt": source.retrieved_at,
        "status": claim.status,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn search(store: &Store, args: SearchArgs) -> serde_json::Value {
    let spans = store.list_spans();
    let sources = store.list_sources();
    let words: Vec<String> = args
        .query
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_lowercase)
        .collect();
    let mut claims: Vec<_> = store
        .list_claims(args.status.as_deref())
        .into_iter()
        .filter(|c| {
            let text = c.text.to_lowercase();
            words.iter().all(|w| text.contains(w))
        })
        .collect();
    // Newest first (ids are sequential).
    claims.reverse();
    let limit = args.limit.unwrap_or(20).max(1);
    let results: Vec<_> = claims
        .into_iter()
        .take(limit)
        .map(|c| {
            let evidence: Vec<_> = c
                .span_ids
                .iter()
                .filter_map(|id| spans.iter().find(|s| &s.id == id))
                .map(|span| {
                    let source = sources.iter().find(|s| s.id == span.source_id);
                    json!({
                        "quote": span.text,
                        "sourceId": span.source_id,
                        "sourceTitle": source.map(|s| s.title.clone()),
                        "sourceUrl": source.and_then(|s| s.origin.clone()),
                        "retrievedAt": source.map(|s| s.retrieved_at.clone()),
                    })
                })
                .collect();
            json!({
                "claimId": c.id,
                "claim": c.text,
                "status": c.status,
                "confidence": c.confidence,
                "evidence": evidence,
            })
        })
        .collect();
    json!({ "count": results.len(), "results": results })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    text: String,
    #[serde(default)]
    source_ids: Option<Vec<String>>,
}

fn verify(store: &Store, args: VerifyArgs) -> Result<serde_json::Value, String> {
    let ids: Vec<String> = match args.source_ids {
        Some(ids) => ids,
        None => store.list_sources().into_iter().map(|s| s.id).collect(),
    };
    let mut texts = Vec::new();
    for id in &ids {
        let (_, text) = store
            .read_source(id)
            .ok_or_else(|| format!("source not found: {id}"))?;
        texts.push(text);
    }
    let findings = codex_inocode_evidence::verify_document(&args.text, &texts);
    Ok(json!({
        "checkedSources": ids.len(),
        "findings": findings,
        "note": "Findings are reports only; nothing was changed or blocked.",
    }))
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
            let (arguments, thread_id) = arguments(invocation, RECORD_EVIDENCE_TOOL_NAME)?;
            let args: RecordArgs = parse_arguments(&arguments)?;
            let created_by = format!("model:{thread_id}");
            record(&store(), args, &created_by).map(out).map_err(model_error)
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
            let (arguments, _) = arguments(invocation, SEARCH_EVIDENCE_TOOL_NAME)?;
            let args: SearchArgs = parse_arguments(&arguments)?;
            Ok(out(search(&store(), args)))
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
            let (arguments, _) = arguments(invocation, VERIFY_EVIDENCE_DOCUMENT_TOOL_NAME)?;
            let args: VerifyArgs = parse_arguments(&arguments)?;
            verify(&store(), args).map(out).map_err(model_error)
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
        let (name, d) = name_and_description(create_search_evidence_tool());
        assert_eq!(name, "search_evidence");
        assert!(d.contains("Check it before looking something up again"));
        let (name, _) = name_and_description(create_verify_evidence_document_tool());
        assert_eq!(name, "verify_evidence_document");
    }

    #[test]
    fn record_search_and_verify_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let rec = |claim: &str, quote: &str, text: Option<&str>| RecordArgs {
            claim: claim.into(),
            quote: quote.into(),
            source_url: Some("https://example.com/price".into()),
            source_title: Some("Price page".into()),
            source_text: text.map(str::to_string),
            claim_type: None,
            status: None,
            confidence: None,
        };
        let page = "Widget X costs 1,200 yen (tax included) as of 2026-11-11.";
        let first = record(&store, rec("Widget X is 1,200 yen", "Widget X costs 1,200 yen", Some(page)), "model:t").unwrap();
        assert_eq!(first["status"], "reported");
        // The same page again is the same source.
        let second = record(&store, rec("Widget X tax included", "tax included", Some(page)), "model:t").unwrap();
        assert_eq!(first["sourceId"], second["sourceId"]);
        // A quote that is not in the text is refused.
        assert!(record(&store, rec("x", "Widget X costs 9 yen", Some(page)), "model:t").is_err());
        // Bad status refused.
        let mut bad = rec("x", "tax included", Some(page));
        bad.status = Some("maybe".into());
        assert!(record(&store, bad, "model:t").is_err());

        let found = search(&store, SearchArgs { query: Some("widget yen".into()), status: None, limit: None });
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["evidence"][0]["sourceUrl"], "https://example.com/price");
        assert!(found["results"][0]["evidence"][0]["retrievedAt"].is_string());
        let none = search(&store, SearchArgs { query: Some("gadget".into()), status: None, limit: None });
        assert_eq!(none["count"], 0);

        let ids = vec![first["sourceId"].as_str().unwrap().to_string()];
        let report = verify(&store, VerifyArgs { text: "Widget X costs 1,200 yen.".into(), source_ids: Some(ids) }).unwrap();
        assert_eq!(report["checkedSources"], 1);
        assert!(verify(&store, VerifyArgs { text: "t".into(), source_ids: Some(vec!["nope".into()]) }).is_err());
    }

    #[test]
    fn arguments_reject_unknown_fields_and_bad_enums() {
        assert!(serde_json::from_str::<RecordArgs>(r#"{"claim":"a","quote":"b","x":1}"#).is_err());
        assert!(
            serde_json::from_str::<RecordArgs>(r#"{"claim":"a","quote":"b","confidence":"high"}"#)
                .is_ok()
        );
    }
}
