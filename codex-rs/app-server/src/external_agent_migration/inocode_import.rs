//! `inocode/import/*`: Claude Code and Codex history into InoCode threads (ref/docs/70).
//!
//! - `detect` `{}` → `{sessions: [...], memories: [...]}` (codex_inocode_import::Found / Memory).
//! - `run` `{path, source}` → `{threadId, title, cwd, existing, brokenLines}`: one session file
//!   becomes a thread (the upstream importer's thread creation), the user's messages go into the
//!   ledger of the user's words. A file imported before and unchanged gives its thread again.
//! - `state` `{threadId, cwd?}` → `{imported, source, path, extraction, ask}`: `ask` when the user
//!   has not decided yet about extracting items from it (asked when the thread is first opened).
//! - `decide` `{threadId, decision: "later" | "started" | "declined", cwd?}`: "declined" stops the
//!   question for that project.
//!
//! Nothing is extracted here: extraction is a turn of the model in the imported thread, with
//! InoCode's own tools, that the user starts.

use super::processor::ExternalAgentConfigRequestProcessor;
use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::InocodeImportDecideResponse;
use codex_app_server_protocol::InocodeImportDetectResponse;
use codex_app_server_protocol::InocodeImportRunResponse;
use codex_app_server_protocol::InocodeImportStateResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_external_agent_migration::sessions::ImportedExternalAgentSession;
use codex_inocode_import as imp;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

fn fields(v: Value) -> HashMap<String, Value> {
    match v {
        Value::Object(m) => m.into_iter().collect(),
        _ => HashMap::new(),
    }
}

fn param(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
}

fn home() -> PathBuf {
    codex_inocode_ledger::inocode_home()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ExternalAgentConfigRequestProcessor {
    pub(crate) async fn inocode_import(
        &self,
        request: ClientRequest,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let params = serde_json::to_value(&request)
            .map_err(|e| internal_error(e.to_string()))?
            .get("params")
            .cloned()
            .unwrap_or(Value::Null);
        let payload: ClientResponsePayload = match &request {
            ClientRequest::InocodeImportDetect { .. } => {
                let value = tokio::task::spawn_blocking(|| {
                    let roots = imp::Roots::from_env();
                    json!({
                        "sessions": imp::detect(&roots, &home()),
                        "memories": imp::detect_memories(&roots),
                    })
                })
                .await
                .map_err(|e| internal_error(e.to_string()))?;
                InocodeImportDetectResponse { fields: fields(value) }.into()
            }
            ClientRequest::InocodeImportRun { .. } => {
                InocodeImportRunResponse { fields: fields(self.inocode_import_run(&params).await?) }.into()
            }
            ClientRequest::InocodeImportState { .. } => {
                let thread = param(&params, "threadId").ok_or_else(|| invalid_request("threadId is required"))?;
                let record = imp::read_record(&home());
                let t = record.threads.get(&thread);
                let value = json!({
                    "imported": t.is_some(),
                    "source": t.and_then(|t| t.source),
                    "path": t.map(|t| t.path.clone()),
                    "cwd": t.map(|t| t.cwd.clone()),
                    "extraction": t.map(|t| t.extraction),
                    "contextTokens": t.map(|t| t.context_tokens),
                    "ask": imp::should_ask(&record, &thread, param(&params, "cwd").as_deref()),
                });
                InocodeImportStateResponse { fields: fields(value) }.into()
            }
            ClientRequest::InocodeImportDecide { .. } => {
                let thread = param(&params, "threadId").ok_or_else(|| invalid_request("threadId is required"))?;
                let decision = match param(&params, "decision").as_deref() {
                    Some("later") => imp::Extraction::Later,
                    Some("started") => imp::Extraction::Started,
                    Some("declined") => imp::Extraction::Declined,
                    other => return Err(invalid_request(format!("unknown decision: {other:?}"))),
                };
                let home = home();
                let mut record = imp::read_record(&home);
                let Some(t) = record.threads.get_mut(&thread) else {
                    return Err(invalid_request("not an imported thread"));
                };
                t.extraction = decision;
                let cwd = param(&params, "cwd").unwrap_or_else(|| t.cwd.clone());
                if decision == imp::Extraction::Declined && !cwd.is_empty() && !record.quiet_projects.contains(&cwd) {
                    record.quiet_projects.push(cwd);
                }
                imp::write_record(&home, &record).map_err(|e| internal_error(e.to_string()))?;
                InocodeImportDecideResponse { fields: fields(json!({ "ok": true })) }.into()
            }
            _ => return Err(internal_error(format!("not an import request: {}", request.method_name()))),
        };
        Ok(Some(payload))
    }

    async fn inocode_import_run(&self, params: &Value) -> Result<Value, JSONRPCErrorError> {
        let path = param(params, "path").ok_or_else(|| invalid_request("path is required"))?;
        let source = match param(params, "source").as_deref() {
            Some("claude") => imp::Source::Claude,
            Some("codex") => imp::Source::Codex,
            other => return Err(invalid_request(format!("unknown source: {other:?}"))),
        };
        let home = home();
        let file = PathBuf::from(&path);
        let read = {
            let file = file.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<(String, u64, imp::Converted)> {
                let sha = imp::sha256_file(&file)?;
                let size = std::fs::metadata(&file)?.len();
                let converted = imp::convert(&file, source)?;
                Ok((sha, size, converted))
            })
            .await
            .map_err(|e| internal_error(e.to_string()))?
            .map_err(|e| invalid_request(format!("cannot read {path}: {e}")))?
        };
        let (sha, size, converted) = read;
        let record = imp::read_record(&home);
        if let Some(done) = record.files.get(&path).filter(|d| d.sha256 == sha) {
            return Ok(json!({ "threadId": done.thread_id, "existing": true }));
        }
        if converted.user_texts.is_empty() {
            return Err(invalid_request("the session has no message of the user"));
        }
        // The session's folder, when it is still there; else the user's home.
        let cwd = converted
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        let title = converted.title.clone();
        let user_texts = converted.user_texts.clone();
        let broken = converted.broken_lines;
        let context_tokens = converted.context_tokens;
        let session = ImportedExternalAgentSession {
            cwd: cwd.clone(),
            title: title.clone(),
            first_user_message: converted.first_user.clone(),
            rollout_items: converted.items,
        };
        let thread_id = self
            .session_importer()
            .persist_session(session)
            .await
            .map_err(|e| internal_error(format!("cannot create the thread: {}", e.message)))?
            .to_string();
        let thread = thread_id.clone();
        let cwd_text = cwd.to_string_lossy().to_string();
        let source_path = path.clone();
        let cwd_saved = cwd_text.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            for text in &user_texts {
                codex_inocode_ledger::record_at(&home, &thread, text);
            }
            let mut record = imp::read_record(&home);
            record.files.insert(
                source_path.clone(),
                imp::ImportedFile { source: Some(source), sha256: sha, size_bytes: size, thread_id: thread.clone(), imported_at: now() },
            );
            record.threads.insert(
                thread,
                imp::ImportedThread { source: Some(source), path: source_path, cwd: cwd_saved, extraction: imp::Extraction::None, context_tokens },
            );
            imp::write_record(&home, &record)
        })
        .await
        .map_err(|e| internal_error(e.to_string()))?
        .map_err(|e| internal_error(e.to_string()))?;
        Ok(json!({
            "threadId": thread_id,
            "title": title,
            "cwd": cwd_text,
            "existing": false,
            "brokenLines": broken,
            "sourcePath": Path::new(&path).to_string_lossy(),
        }))
    }
}
