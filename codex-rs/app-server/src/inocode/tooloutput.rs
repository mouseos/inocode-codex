//! `inocode/toolOutput/list` and `inocode/toolOutput/read`: large tool outputs spilled to
//! `<INOCODE_HOME>/tool-results/` by `codex-inocode-tool-output`.

use super::InocodeResult;
use super::unknown_method;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InocodeSessionsSlimParams;
use codex_app_server_protocol::InocodeSessionsSlimResponse;
use codex_app_server_protocol::InocodeToolOutputItem;
use codex_app_server_protocol::InocodeToolOutputListParams;
use codex_app_server_protocol::InocodeToolOutputListResponse;
use codex_app_server_protocol::InocodeToolOutputReadParams;
use codex_app_server_protocol::InocodeToolOutputReadResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use std::path::Path;
use std::path::PathBuf;

const DEFAULT_LIMIT: usize = 100;

fn results_dir() -> PathBuf {
    codex_inocode_flags::inocode_home().join("tool-results")
}

fn num(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) fn list_in(
    dir: &Path,
    params: &InocodeToolOutputListParams,
) -> InocodeToolOutputListResponse {
    let all = codex_inocode_tool_output::list_outputs(dir);
    let limit = if params.limit == 0 {
        DEFAULT_LIMIT
    } else {
        params.limit as usize
    };
    let offset = params
        .cursor
        .as_deref()
        .and_then(|c| c.parse::<usize>().ok())
        .unwrap_or(0);
    let items: Vec<InocodeToolOutputItem> = all
        .iter()
        .skip(offset)
        .take(limit)
        .map(|e| InocodeToolOutputItem {
            id: e.id.clone(),
            ts_ms: num(e.ts_ms),
            bytes: num(e.bytes),
            approx_tokens: num(e.approx_tokens),
            tool: e.tool.clone(),
            thread_id: e.thread_id.clone(),
            call_id: e.call_id.clone(),
        })
        .collect();
    let next_cursor =
        (offset + items.len() < all.len()).then(|| (offset + items.len()).to_string());
    InocodeToolOutputListResponse { items, next_cursor }
}

pub(crate) fn read_in(
    dir: &Path,
    params: &InocodeToolOutputReadParams,
) -> Result<InocodeToolOutputReadResponse, JSONRPCErrorError> {
    if !codex_inocode_tool_output::is_valid_id(&params.id) {
        return Err(invalid_params(format!(
            "invalid tool output id: {}",
            params.id
        )));
    }
    let slice = codex_inocode_tool_output::read_output(
        dir,
        &params.id,
        params.offset.unwrap_or(0) as usize,
        params.limit.map(|l| l as usize),
    )
    .map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            invalid_params(format!("tool output not found: {}", params.id))
        } else {
            internal_error(format!("failed to read tool output {}: {err}", params.id))
        }
    })?;
    Ok(InocodeToolOutputReadResponse {
        id: params.id.clone(),
        bytes: num(slice.bytes),
        text: slice.text,
        truncated: slice.truncated,
    })
}

pub(crate) fn slim_in(
    params: &InocodeSessionsSlimParams,
) -> Result<InocodeSessionsSlimResponse, JSONRPCErrorError> {
    let src = PathBuf::from(&params.path);
    if !src.is_file() {
        return Err(invalid_params(format!("rollout not found: {}", params.path)));
    }
    let home = codex_inocode_flags::inocode_home();
    let dst = params
        .output_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_inocode_tool_output::default_slim_destination(&home, &src));
    let mut options = codex_inocode_tool_output::SlimOptions::default();
    if let Some(bytes) = params.part_threshold_bytes {
        options.part_threshold_bytes = bytes as usize;
    }
    options.preview_bytes = usize::try_from(codex_inocode_flags::int_param(
        "compaction.preview_bytes",
    ))
    .unwrap_or(options.preview_bytes);
    let report = codex_inocode_tool_output::slim_rollout(&src, &dst, &results_dir(), &options)
        .map_err(|err| internal_error(format!("sessions slim failed: {err}")))?;
    Ok(InocodeSessionsSlimResponse {
        output_path: dst.display().to_string(),
        lines: num(report.lines),
        rewritten_lines: num(report.rewritten_lines),
        moved_texts: num(report.moved_texts),
        moved_images: num(report.moved_images),
        bytes_before: num(report.bytes_before),
        bytes_after: num(report.bytes_after),
    })
}

pub(crate) async fn handle(request: ClientRequest) -> InocodeResult {
    match request {
        ClientRequest::InocodeToolOutputList { params, .. } => {
            Ok(Some(list_in(&results_dir(), &params).into()))
        }
        ClientRequest::InocodeToolOutputRead { params, .. } => {
            Ok(Some(read_in(&results_dir(), &params)?.into()))
        }
        ClientRequest::InocodeSessionsSlim { params, .. } => {
            // Streams a possibly multi-GB file, so keep it off the async threads.
            let response = tokio::task::spawn_blocking(move || slim_in(&params))
                .await
                .map_err(|err| internal_error(format!("sessions slim failed to join: {err}")))??;
            Ok(Some(response.into()))
        }
        other => Err(unknown_method(other.method_name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_inocode_tool_output::PersistConfig;
    use codex_inocode_tool_output::PersistMeta;

    #[test]
    fn list_and_read_a_persisted_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = PersistConfig::new(true, 10, dir.path());
        let text = "line\n".repeat(400);
        codex_inocode_tool_output::persist_if_large_with(
            &text,
            &cfg,
            &PersistMeta {
                tool: Some("shell".into()),
                thread_id: None,
                call_id: Some("call-1".into()),
            },
        )
        .expect("persisted");
        let listed = list_in(
            &cfg.dir,
            &InocodeToolOutputListParams {
                limit: 10,
                cursor: None,
            },
        );
        assert_eq!(listed.items.len(), 1);
        let item = &listed.items[0];
        assert_eq!(item.call_id.as_deref(), Some("call-1"));
        assert_eq!(item.bytes, 2000);
        let full = read_in(
            &cfg.dir,
            &InocodeToolOutputReadParams {
                id: item.id.clone(),
                offset: None,
                limit: None,
            },
        )
        .expect("read");
        assert_eq!(full.text, text);
        assert!(!full.truncated);
        let part = read_in(
            &cfg.dir,
            &InocodeToolOutputReadParams {
                id: item.id.clone(),
                offset: Some(5),
                limit: Some(5),
            },
        )
        .expect("read");
        assert_eq!(part.text, "line\n");
        assert!(part.truncated);
        assert!(
            read_in(
                &cfg.dir,
                &InocodeToolOutputReadParams {
                    id: "../secret".into(),
                    offset: None,
                    limit: None
                }
            )
            .is_err()
        );
    }
}
