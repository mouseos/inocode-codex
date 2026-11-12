//! Restore points for file changes in the working folder (`ext/inocode-checkpoint`).
//!
//! Called before `apply_patch` changes files and before a shell command runs. A restore point never
//! stops or slows the change beyond its own time budget: when one cannot be made, the change goes
//! ahead and one warning line says so.

use std::path::PathBuf;
use std::time::Duration;

use codex_inocode_checkpoint::Captured;
use codex_inocode_checkpoint::Config;
use codex_inocode_checkpoint::Origin;
use codex_inocode_checkpoint::Store;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

pub(crate) fn config() -> Config {
    Config {
        max_file_bytes: codex_inocode_flags::int_param("checkpoint.max_file_bytes"),
        max_untracked_dir_files: codex_inocode_flags::int_param("checkpoint.max_untracked_dir_files")
            as usize,
        budget: Duration::from_secs(codex_inocode_flags::int_param("checkpoint.budget_secs")),
    }
}

pub(crate) fn store() -> Store {
    Store::at_home(&codex_inocode_flags::inocode_home())
}

fn enabled() -> bool {
    codex_inocode_flags::InocodeFlags::load().checkpoint_enabled
}

async fn run(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    capture: impl FnOnce(&Store, &Origin, &Config) -> Captured + Send + 'static,
) {
    if !enabled() {
        return;
    }
    let origin = Origin {
        thread_id: session.thread_id.to_string(),
        turn_id: turn.sub_id.clone(),
        call_id: call_id.to_string(),
        cwd: crate::compact_inocode::local_cwd(turn),
    };
    let thread_id = origin.thread_id.clone();
    let captured = tokio::task::spawn_blocking(move || capture(&store(), &origin, &config())).await;
    let metrics_on = codex_inocode_flags::InocodeFlags::load().metrics_enabled;
    match captured {
        Ok(Captured::Made(point)) if metrics_on => {
            codex_inocode_metrics::record_global(
                &thread_id,
                codex_inocode_metrics::MetricEvent::CheckpointMade {
                    files: point.stats.files,
                    bytes_written: point.stats.bytes_written,
                    millis: point.stats.millis,
                },
            );
        }
        Ok(Captured::Omitted(line)) => {
            if metrics_on {
                codex_inocode_metrics::record_global(
                    &thread_id,
                    codex_inocode_metrics::MetricEvent::CheckpointOmitted,
                );
            }
            session
                .send_event(turn, EventMsg::Warning(WarningEvent { message: line }))
                .await;
        }
        _ => {}
    }
}

/// Before `apply_patch` changes `paths`.
pub(crate) async fn before_apply_patch(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    paths: Vec<PathBuf>,
) {
    run(session, turn, call_id, move |store, origin, cfg| {
        codex_inocode_checkpoint::capture_paths(store, origin, &paths, cfg)
    })
    .await;
}

/// Before a shell command runs.
pub(crate) async fn before_shell(session: &Session, turn: &TurnContext, call_id: &str) {
    run(session, turn, call_id, |store, origin, cfg| {
        codex_inocode_checkpoint::capture_shell(store, origin, cfg)
    })
    .await;
}
