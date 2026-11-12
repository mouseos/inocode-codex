//! `inocode/kb/*`: the knowledge redesign (doc 60/61), backed by `codex_inocode_knowledge::kb`.

use super::InocodeResult;
use crate::error_code::internal_error;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeKbProjectGetResponse;
use codex_app_server_protocol::InocodeKbOverviewReadResponse;
use codex_app_server_protocol::InocodeKbOverviewWriteResponse;
use codex_app_server_protocol::InocodeKbIndexGetResponse;
use codex_app_server_protocol::InocodeKbPageListResponse;
use codex_app_server_protocol::InocodeKbPageReadResponse;
use codex_app_server_protocol::InocodeKbPageWriteResponse;
use codex_app_server_protocol::InocodeKbPageDeleteResponse;
use codex_app_server_protocol::InocodeKbTagListResponse;
use codex_app_server_protocol::InocodeKbProhibitionListResponse;
use codex_app_server_protocol::InocodeKbProhibitionWriteResponse;
use codex_app_server_protocol::InocodeKbProhibitionRetractResponse;
use codex_app_server_protocol::InocodeKbRelaxationAddResponse;
use codex_app_server_protocol::InocodeKbRelaxationRevokeResponse;
use codex_app_server_protocol::InocodeKbLedgerListResponse;
use codex_app_server_protocol::InocodeKbLedgerGetResponse;
use codex_app_server_protocol::InocodeKbChecklistListResponse;
use codex_app_server_protocol::InocodeKbChecklistWriteResponse;
use codex_app_server_protocol::InocodeKbChecklistSetDoneResponse;
use codex_app_server_protocol::InocodeKbLegacyRulesResponse;
use codex_app_server_protocol::InocodeKbLegacyMigrateResponse;
use codex_app_server_protocol::InocodeKbLegacyFailureCasesResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_inocode_knowledge::kb;
use serde_json::Value;
use std::collections::HashMap;

fn to_error(e: kb::KbError) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: e.code,
        message: e.message,
        data: e.data,
    }
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    let method = request.method_name();
    let sub = method.strip_prefix("inocode/kb/").unwrap_or_default().to_string();
    let params = match serde_json::to_value(&request) {
        Ok(v) => v.get("params").cloned().unwrap_or(Value::Null),
        Err(e) => return Err(internal_error(e.to_string())),
    };
    // The work reads and writes files; keep it off the async threads.
    let value = tokio::task::spawn_blocking(move || kb::call(&sub, &params))
        .await
        .map_err(|e| internal_error(e.to_string()))?
        .map_err(to_error)?;
    let fields: HashMap<String, Value> = match value {
        Value::Object(m) => m.into_iter().collect(),
        _ => HashMap::new(),
    };
    Ok(Some(match &request {
        ClientRequest::InocodeKbProjectGet { .. } => InocodeKbProjectGetResponse { fields }.into(),
        ClientRequest::InocodeKbOverviewRead { .. } => InocodeKbOverviewReadResponse { fields }.into(),
        ClientRequest::InocodeKbOverviewWrite { .. } => InocodeKbOverviewWriteResponse { fields }.into(),
        ClientRequest::InocodeKbIndexGet { .. } => InocodeKbIndexGetResponse { fields }.into(),
        ClientRequest::InocodeKbPageList { .. } => InocodeKbPageListResponse { fields }.into(),
        ClientRequest::InocodeKbPageRead { .. } => InocodeKbPageReadResponse { fields }.into(),
        ClientRequest::InocodeKbPageWrite { .. } => InocodeKbPageWriteResponse { fields }.into(),
        ClientRequest::InocodeKbPageDelete { .. } => InocodeKbPageDeleteResponse { fields }.into(),
        ClientRequest::InocodeKbTagList { .. } => InocodeKbTagListResponse { fields }.into(),
        ClientRequest::InocodeKbProhibitionList { .. } => InocodeKbProhibitionListResponse { fields }.into(),
        ClientRequest::InocodeKbProhibitionWrite { .. } => InocodeKbProhibitionWriteResponse { fields }.into(),
        ClientRequest::InocodeKbProhibitionRetract { .. } => InocodeKbProhibitionRetractResponse { fields }.into(),
        ClientRequest::InocodeKbRelaxationAdd { .. } => InocodeKbRelaxationAddResponse { fields }.into(),
        ClientRequest::InocodeKbRelaxationRevoke { .. } => InocodeKbRelaxationRevokeResponse { fields }.into(),
        ClientRequest::InocodeKbLedgerList { .. } => InocodeKbLedgerListResponse { fields }.into(),
        ClientRequest::InocodeKbLedgerGet { .. } => InocodeKbLedgerGetResponse { fields }.into(),
        ClientRequest::InocodeKbChecklistList { .. } => InocodeKbChecklistListResponse { fields }.into(),
        ClientRequest::InocodeKbChecklistWrite { .. } => InocodeKbChecklistWriteResponse { fields }.into(),
        ClientRequest::InocodeKbChecklistSetDone { .. } => InocodeKbChecklistSetDoneResponse { fields }.into(),
        ClientRequest::InocodeKbLegacyRules { .. } => InocodeKbLegacyRulesResponse { fields }.into(),
        ClientRequest::InocodeKbLegacyMigrate { .. } => InocodeKbLegacyMigrateResponse { fields }.into(),
        ClientRequest::InocodeKbLegacyFailureCases { .. } => InocodeKbLegacyFailureCasesResponse { fields }.into(),
        _ => return Err(internal_error("not a kb request".to_string())),
    }))
}
