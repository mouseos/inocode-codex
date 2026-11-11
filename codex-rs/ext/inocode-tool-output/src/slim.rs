//! Rewrites an existing, possibly enormous rollout into a small one, one line at a time.
//!
//! Memory is one line (plus the biggest text being moved out), however large the file is. Lines
//! below the threshold are copied byte for byte. For a bigger line, each oversized tool-output
//! text goes to `tool-results/` (the line keeps the same preview text the model would see after a
//! resume) and each inline image goes to an image file (the line keeps a text note with its path).
//! The source file is only read; the result goes to a new file.

use crate::PersistConfig;
use crate::PersistMeta;
use serde_json::Value;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SlimOptions {
    /// Only lines at least this long are parsed and rewritten.
    pub line_threshold_bytes: usize,
    /// A text part or image at least this long is moved out of the line.
    pub part_threshold_bytes: usize,
    pub preview_bytes: usize,
}

impl Default for SlimOptions {
    fn default() -> Self {
        Self {
            line_threshold_bytes: 64 * 1024,
            part_threshold_bytes: 32 * 1024,
            preview_bytes: crate::DEFAULT_PREVIEW_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlimReport {
    pub lines: u64,
    pub rewritten_lines: u64,
    pub moved_texts: u64,
    pub moved_images: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Streams `src` into the new file `dst` (it must not exist), moving oversized outputs into `dir`.
pub fn slim_rollout(
    src: &Path,
    dst: &Path,
    dir: &Path,
    options: &SlimOptions,
) -> std::io::Result<SlimReport> {
    let mut input = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(src)?);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let mut output = std::io::BufWriter::with_capacity(1 << 20, output);
    let mut report = SlimReport::default();
    let mut config = PersistConfig::new(
        true,
        options.part_threshold_bytes / 4,
        dir.parent().unwrap_or(dir),
    );
    config.dir = dir.to_path_buf();
    config.preview_bytes = options.preview_bytes;
    let meta = PersistMeta::default();

    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        let read = input.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        report.lines += 1;
        report.bytes_before += read as u64;
        let had_newline = line.last() == Some(&b'\n');
        let body_len = line.len() - usize::from(had_newline);
        let mut rewritten: Option<Vec<u8>> = None;
        if body_len >= options.line_threshold_bytes
            && let Ok(mut value) = serde_json::from_slice::<Value>(&line[..body_len])
        {
            let (texts, images) = slim_value(&mut value, &config, &meta, options, dir);
            if texts + images > 0 {
                report.moved_texts += texts;
                report.moved_images += images;
                report.rewritten_lines += 1;
                rewritten = serde_json::to_vec(&value).ok();
            }
        }
        match rewritten {
            Some(bytes) => {
                output.write_all(&bytes)?;
                output.write_all(b"\n")?;
                report.bytes_after += bytes.len() as u64 + 1;
            }
            None => {
                output.write_all(&line)?;
                report.bytes_after += line.len() as u64;
            }
        }
    }
    output.flush()?;
    Ok(report)
}

fn is_output_type(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    )
}

/// Rewrites one rollout record in place. Returns (texts moved, images moved).
fn slim_value(
    value: &mut Value,
    config: &PersistConfig,
    meta: &PersistMeta,
    options: &SlimOptions,
    dir: &Path,
) -> (u64, u64) {
    let mut counts = (0u64, 0u64);
    let Some(payload) = value.get_mut("payload") else {
        return counts;
    };
    if is_output_type(payload) {
        slim_output(payload, config, meta, options, dir, &mut counts);
    }
    // A compaction carries the replacement history, whose items have the same shape.
    if let Some(Value::Array(items)) = payload.get_mut("replacement_history") {
        for item in items {
            if is_output_type(item) {
                slim_output(item, config, meta, options, dir, &mut counts);
            }
        }
    }
    counts
}

fn slim_text(
    text: &mut String,
    config: &PersistConfig,
    meta: &PersistMeta,
    options: &SlimOptions,
    counts: &mut (u64, u64),
) {
    if text.len() >= options.part_threshold_bytes
        && let Some(replacement) = crate::persist_if_large_with(text, config, meta)
    {
        *text = replacement;
        counts.0 += 1;
    }
}

fn slim_output(
    item: &mut Value,
    config: &PersistConfig,
    meta: &PersistMeta,
    options: &SlimOptions,
    dir: &Path,
    counts: &mut (u64, u64),
) {
    let Some(output) = item.get_mut("output") else {
        return;
    };
    match output {
        Value::String(text) => slim_text(text, config, meta, options, counts),
        Value::Array(parts) => {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).map(str::to_string);
                match kind.as_deref() {
                    Some("input_text" | "output_text") => {
                        if let Some(Value::String(text)) = part.get_mut("text") {
                            slim_text(text, config, meta, options, counts);
                        }
                    }
                    Some("input_image") => {
                        let url = part
                            .get("image_url")
                            .and_then(Value::as_str)
                            .filter(|url| url.len() >= options.part_threshold_bytes)
                            .map(str::to_string);
                        if let Some(url) = url
                            && let Some(path) = crate::persist_image_data_url(dir, &url)
                        {
                            *part = serde_json::json!({
                                "type": "input_text",
                                "text": format!(
                                    "[image ({} bytes) saved to: {}]",
                                    url.len(),
                                    path.display()
                                ),
                            });
                            counts.1 += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Default destination for a slimmed copy of `src`: `<home>/slimmed/<file name>`.
pub fn default_slim_destination(inocode_home: &Path, src: &Path) -> PathBuf {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rollout.jsonl".into());
    inocode_home.join("slimmed").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(value: Value) -> String {
        format!("{}\n", serde_json::to_string(&value).unwrap())
    }

    #[test]
    fn slims_big_outputs_and_images_and_keeps_the_rest_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.jsonl");
        let dst = dir.path().join("out.jsonl");
        let results = dir.path().join("tool-results");
        let small =
            line(serde_json::json!({"timestamp":"t","type":"session_meta","payload":{"id":"x"}}));
        let big_text = "line of output\n".repeat(20_000);
        let text_line = line(serde_json::json!({"timestamp":"t","type":"response_item","payload":{
            "type":"function_call_output","call_id":"c1","output": big_text}}));
        let image = format!("data:image/png;base64,{}", "QUJD".repeat(50_000));
        let image_line = line(serde_json::json!({"timestamp":"t","type":"response_item","payload":{
            "type":"custom_tool_call_output","call_id":"c2","output":[
                {"type":"input_text","text":"shot"},
                {"type":"input_image","image_url": image}]}}));
        std::fs::write(&src, format!("{small}{text_line}{image_line}{small}")).unwrap();

        let report = slim_rollout(&src, &dst, &results, &SlimOptions::default()).unwrap();
        assert_eq!(report.lines, 4);
        assert_eq!(report.rewritten_lines, 2);
        assert_eq!((report.moved_texts, report.moved_images), (1, 1));
        assert!(report.bytes_after < report.bytes_before / 10);

        let out = std::fs::read_to_string(&dst).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        // Small lines are copied byte for byte.
        assert_eq!(format!("{}\n", lines[0]), small);
        assert_eq!(format!("{}\n", lines[3]), small);
        let v1: Value = serde_json::from_str(lines[1]).unwrap();
        let t = v1["payload"]["output"].as_str().unwrap();
        assert!(crate::is_persisted_output(t));
        let v2: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(v2["payload"]["output"][0]["text"], "shot");
        assert!(
            v2["payload"]["output"][1]["text"]
                .as_str()
                .unwrap()
                .starts_with("[image (")
        );
        // The originals are in tool-results/.
        let names: Vec<String> = std::fs::read_dir(&results)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n.ends_with(".txt") && !n.starts_with("index")));
        assert!(names.iter().any(|n| n.ends_with(".png")));
        // The source is untouched, and an existing destination is refused.
        assert!(std::fs::read_to_string(&src).unwrap().contains(&big_text[..100]));
        assert!(slim_rollout(&src, &dst, &results, &SlimOptions::default()).is_err());
    }
}
