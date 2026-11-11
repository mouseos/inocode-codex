//! Model-id normalisation for matching across sources.

/// The provider name the public sources use for an InoCode provider id.
pub(crate) fn source_provider(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "anthropic".to_string(),
        other => other.to_string(),
    }
}

/// Lower case, and a `.` between two digits is a `-` (`gpt-5.4-mini` and `gpt-5-4-mini` are the
/// same model). Nothing else changes: `-pro`, `-batch`, `:free`, `-preview` and date suffixes
/// stay, so those are different models.
pub(crate) fn normalize_model(id: &str) -> String {
    let lower = id.trim().to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = String::with_capacity(lower.len());
    for (index, c) in chars.iter().enumerate() {
        let between_digits = *c == '.'
            && index > 0
            && chars[index - 1].is_ascii_digit()
            && chars.get(index + 1).is_some_and(char::is_ascii_digit);
        out.push(if between_digits { '-' } else { *c });
    }
    out
}

/// `anthropic/claude-sonnet-5.5` -> (`anthropic`, `claude-sonnet-5.5`). An id without a provider
/// part has none.
pub(crate) fn split_provider(id: &str) -> (Option<&str>, &str) {
    match id.split_once('/') {
        Some((provider, rest)) if !provider.is_empty() && !rest.is_empty() => (Some(provider), rest),
        _ => (None, id),
    }
}
