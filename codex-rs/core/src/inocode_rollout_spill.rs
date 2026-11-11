//! InoCode: keep huge tool outputs out of the rollout (and out of memory on every later read).
//!
//! The rollout used to carry each tool output verbatim, so a few multi-megabyte outputs made a
//! session file hundreds of MB and every full read held them all. Before an output is recorded,
//! an oversized text part is written to `INOCODE_HOME/tool-results/<id>.txt` and replaced by the
//! same preview-plus-path text the model would see after a resume (`compaction.persist_large_output`).
//! Older rollouts with long lines still load unchanged.
//!
//! Screenshots and other inline images in tool outputs are the bulk of the very large lines (a
//! few MB of base64 each). Only the rollout copy is rewritten: the image goes to
//! `tool-results/` as a file and the rollout keeps a text note with its path, so the live
//! turn (and clients) still get the real image.
//!
//! The threshold is the history truncation budget (the same one the history uses), unless
//! `rollout.large_output_bytes` is set.

use codex_history::ResponseItemEnvelope;
use codex_inocode_tool_output::PersistConfig;
use codex_inocode_tool_output::PersistMeta;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;

/// Spills oversized text parts of tool outputs in `items`. A no-op when
/// `compaction.persist_large_output` is off or nothing is large.
pub(crate) fn spill_large_outputs(items: &mut [ResponseItemEnvelope]) {
    // Cheap pre-pass so the flag file is read only when something could qualify.
    let any_large = items.iter().any(|envelope| match output_of(&envelope.item) {
        Some(output) => payload_max_text_len(output) > 4096,
        None => false,
    });
    if !any_large {
        return;
    }
    if !codex_inocode_flags::InocodeFlags::load().compaction_persist_large_output {
        return;
    }
    let home = codex_inocode_flags::inocode_home();
    let override_bytes = codex_inocode_flags::int_param("rollout.large_output_bytes");
    let preview_bytes = usize::try_from(codex_inocode_flags::int_param("compaction.preview_bytes"))
        .unwrap_or(usize::MAX);
    for envelope in items {
        let budget = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.history_truncation_token_limit);
        let (call_id, output) = match &mut envelope.item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => (call_id.clone().unwrap_or_default(), output),
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => (call_id.clone(), output),
            _ => continue,
        };
        let threshold_tokens = if override_bytes > 0 {
            usize::try_from(override_bytes / 4).unwrap_or(usize::MAX).max(1)
        } else if let Some(budget) = budget {
            budget
        } else {
            continue;
        };
        let mut config = PersistConfig::new(true, threshold_tokens, &home);
        config.preview_bytes = preview_bytes;
        let meta = PersistMeta {
            tool: None,
            thread_id: None,
            call_id: (!call_id.is_empty()).then_some(call_id),
        };
        spill_payload(output, &config, &meta);
    }
}

/// Moves large inline images of tool outputs to files and leaves a text note with the path,
/// in the items that are about to be written to the rollout (the history already holds the
/// originals). A no-op when `compaction.persist_large_output` is off.
pub(crate) fn externalize_images(items: &mut [ResponseItemEnvelope]) {
    let has_image = items.iter().any(|envelope| {
        output_of(&envelope.item).is_some_and(|output| match &output.body {
            FunctionCallOutputBody::ContentItems(parts) => parts
                .iter()
                .any(|part| matches!(part, FunctionCallOutputContentItem::InputImage { .. })),
            FunctionCallOutputBody::Text(_) => false,
        })
    });
    if !has_image
        || !codex_inocode_flags::InocodeFlags::load().compaction_persist_large_output
    {
        return;
    }
    let dir = codex_inocode_flags::inocode_home().join("tool-results");
    let override_bytes = codex_inocode_flags::int_param("rollout.large_output_bytes");
    for envelope in items {
        let threshold_bytes = if override_bytes > 0 {
            usize::try_from(override_bytes).unwrap_or(usize::MAX)
        } else if let Some(budget) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.history_truncation_token_limit)
        {
            budget.saturating_mul(4)
        } else {
            continue;
        };
        let output = match &mut envelope.item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => output,
            _ => continue,
        };
        externalize_payload_images(output, &dir, threshold_bytes);
    }
}

/// Replaces each inline image above `threshold_bytes` with a text note. Returns how many.
pub(crate) fn externalize_payload_images(
    output: &mut FunctionCallOutputPayload,
    dir: &std::path::Path,
    threshold_bytes: usize,
) -> usize {
    let FunctionCallOutputBody::ContentItems(parts) = &mut output.body else {
        return 0;
    };
    let mut moved = 0;
    for part in parts {
        let FunctionCallOutputContentItem::InputImage {
            image: ImageReference::Inline { image_url },
            ..
        } = part
        else {
            continue;
        };
        if image_url.len() <= threshold_bytes {
            continue;
        }
        let Some(path) = write_image_file(dir, image_url) else {
            continue;
        };
        let note = format!(
            "[image ({} bytes) saved to: {}]",
            image_url.len(),
            path.display()
        );
        *part = FunctionCallOutputContentItem::InputText { text: note };
        moved += 1;
    }
    moved
}

fn write_image_file(dir: &std::path::Path, data_url: &str) -> Option<std::path::PathBuf> {
    codex_inocode_tool_output::persist_image_data_url(dir, data_url)
}

fn output_of(item: &ResponseItem) -> Option<&FunctionCallOutputPayload> {
    match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => Some(output),
        _ => None,
    }
}

fn payload_max_text_len(output: &FunctionCallOutputPayload) -> usize {
    match &output.body {
        FunctionCallOutputBody::Text(text) => text.len(),
        FunctionCallOutputBody::ContentItems(parts) => parts
            .iter()
            .map(|part| match part {
                FunctionCallOutputContentItem::InputText { text } => text.len(),
                _ => 0,
            })
            .max()
            .unwrap_or(0),
    }
}

/// Replaces each oversized text part of `output` with the preview text. Returns how many parts
/// were spilled.
pub(crate) fn spill_payload(
    output: &mut FunctionCallOutputPayload,
    config: &PersistConfig,
    meta: &PersistMeta,
) -> usize {
    let mut spilled = 0;
    match &mut output.body {
        FunctionCallOutputBody::Text(text) => {
            if let Some(replacement) = codex_inocode_tool_output::persist_if_large_with(text, config, meta) {
                *text = replacement;
                spilled += 1;
            }
        }
        FunctionCallOutputBody::ContentItems(parts) => {
            for part in parts {
                if let FunctionCallOutputContentItem::InputText { text } = part
                    && let Some(replacement) =
                        codex_inocode_tool_output::persist_if_large_with(text, config, meta)
                {
                    *text = replacement;
                    spilled += 1;
                }
            }
        }
    }
    spilled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(n: usize) -> String {
        "line of output\n".repeat(n)
    }

    #[test]
    fn spills_text_and_content_items_and_leaves_small_ones() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistConfig::new(true, 1000, dir.path());
        let meta = PersistMeta::default();

        let mut text = FunctionCallOutputPayload::from_text(big(5000));
        assert_eq!(spill_payload(&mut text, &cfg, &meta), 1);
        let FunctionCallOutputBody::Text(t) = &text.body else { panic!() };
        assert!(codex_inocode_tool_output::is_persisted_output(t));
        assert!(t.len() < 5000);

        let mut parts = FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText { text: "small".into() },
            FunctionCallOutputContentItem::InputText { text: big(5000) },
        ]);
        assert_eq!(spill_payload(&mut parts, &cfg, &meta), 1);
        let FunctionCallOutputBody::ContentItems(p) = &parts.body else { panic!() };
        let FunctionCallOutputContentItem::InputText { text: a } = &p[0] else { panic!() };
        let FunctionCallOutputContentItem::InputText { text: b } = &p[1] else { panic!() };
        assert_eq!(a, "small");
        assert!(codex_inocode_tool_output::is_persisted_output(b));

        // Idempotent: a spilled output is not spilled again.
        assert_eq!(spill_payload(&mut parts, &cfg, &meta), 0);

        // The full text round-trips through the saved file.
        let files = std::fs::read_dir(dir.path().join("tool-results"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "txt"))
            .count();
        assert_eq!(files, 2);
    }

    #[test]
    fn large_images_become_a_note_and_a_file() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![7u8; 5000];
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        let mut payload = FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText { text: "shot".into() },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline { image_url: url },
                detail: Some(codex_protocol::models::ImageDetail::High),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline { image_url: "data:image/png;base64,AA==".into() },
                detail: None,
            },
        ]);
        assert_eq!(externalize_payload_images(&mut payload, dir.path(), 1000), 1);
        let FunctionCallOutputBody::ContentItems(p) = &payload.body else { panic!() };
        let FunctionCallOutputContentItem::InputText { text } = &p[1] else { panic!("not replaced") };
        assert!(text.starts_with("[image (") && text.contains(".png]"));
        // The small image is untouched.
        assert!(matches!(p[2], FunctionCallOutputContentItem::InputImage { .. }));
        let saved = std::fs::read_dir(dir.path()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read(saved).unwrap(), bytes);
    }
}
