//! `inocode/checkpoint/list` and `inocode/checkpoint/restore`: restore points for file changes in the
//! working folder (`codex-inocode-checkpoint`).

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeCheckpointItem;
use codex_app_server_protocol::InocodeCheckpointListParams;
use codex_app_server_protocol::InocodeCheckpointListResponse;
use codex_app_server_protocol::InocodeCheckpointRestoreParams;
use codex_app_server_protocol::InocodeCheckpointRestoreResponse;
use codex_app_server_protocol::InocodeCheckpointSkipped;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_inocode_checkpoint::EntryState;
use codex_inocode_checkpoint::Kind;
use codex_inocode_checkpoint::Point;
use codex_inocode_checkpoint::Store;
use codex_inocode_checkpoint::Target;

fn num(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn item(p: Point) -> InocodeCheckpointItem {
    let mut paths = Vec::new();
    let mut skipped = Vec::new();
    for e in p.entries {
        match e.state {
            EntryState::Saved { .. } => paths.push(e.path),
            EntryState::Absent => {}
            EntryState::Skipped { reason } => skipped.push(InocodeCheckpointSkipped {
                path: e.path,
                reason,
            }),
        }
    }
    InocodeCheckpointItem {
        id: p.id,
        thread_id: p.thread_id,
        turn_id: p.turn_id,
        call_id: p.call_id,
        ts_ms: num(p.ts_ms),
        kind: match p.kind {
            Kind::ApplyPatch => "applyPatch",
            Kind::Shell => "shell",
            Kind::Safety => "safety",
        }
        .to_string(),
        root: p.root,
        files: num(p.stats.files),
        bytes_written: num(p.stats.bytes_written),
        millis: num(p.stats.millis),
        paths,
        skipped,
    }
}

pub(crate) fn list_in(
    store: &Store,
    params: &InocodeCheckpointListParams,
) -> InocodeCheckpointListResponse {
    InocodeCheckpointListResponse {
        items: store
            .list(&params.thread_id)
            .into_iter()
            .filter(|p| params.turn_id.as_ref().is_none_or(|t| &p.turn_id == t))
            .map(item)
            .collect(),
    }
}

pub(crate) fn restore_in(
    store: &Store,
    params: &InocodeCheckpointRestoreParams,
) -> Result<InocodeCheckpointRestoreResponse, JSONRPCErrorError> {
    let target = match (&params.turn_id, &params.point_id) {
        (Some(turn), None) => Target::Turn(turn.clone()),
        (None, Some(point)) => Target::Point(point.clone()),
        _ => return Err(invalid_params("give exactly one of turnId and pointId")),
    };
    let outcome = codex_inocode_checkpoint::restore(
        store,
        &params.thread_id,
        &target,
        &codex_inocode_checkpoint::Config::default(),
    )
    .map_err(invalid_params)?;
    Ok(InocodeCheckpointRestoreResponse {
        restored: outcome.restored,
        removed: outcome.removed,
        skipped: outcome.skipped,
        safety_point_id: outcome.safety_point_id,
        from_points: outcome.from_points,
    })
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    let store = Store::at_home(&codex_inocode_flags::inocode_home());
    match request {
        ClientRequest::InocodeCheckpointList { params, .. } => {
            let response = tokio::task::spawn_blocking(move || list_in(&store, &params))
                .await
                .map_err(|err| internal_error(format!("checkpoint list failed: {err}")))?;
            Ok(Some(response.into()))
        }
        ClientRequest::InocodeCheckpointRestore { params, .. } => {
            let response = tokio::task::spawn_blocking(move || restore_in(&store, &params))
                .await
                .map_err(|err| internal_error(format!("checkpoint restore failed: {err}")))??;
            Ok(Some(response.into()))
        }
        other => Err(unknown_method(other.method_name().as_ref())),
    }
}
