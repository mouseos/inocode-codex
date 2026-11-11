//! Names the field in an argument type error. serde reports `invalid type: map, expected a
//! string` without saying which field it was, and a model that sees only that tends to repeat
//! the same call. The field is found among the top-level keys of the arguments.

use serde_json::Value;

/// The JSON kind serde's "invalid type" wording stands for.
fn kind_of_serde_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "map" => "object",
        "sequence" => "array",
        "string" => "string",
        "integer" | "floating" => "number",
        "boolean" => "boolean",
        "null" | "unit" => "null",
        _ => return None,
    })
}

fn kind_of_value(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    }
}

/// A sentence to add after a serde "invalid type" error: the field (or the candidate fields)
/// holding a value of the wrong kind, and the shape that was expected. `None` when the error is
/// another kind or the arguments are not a JSON object.
pub(crate) fn describe_type_error(arguments: &str, error: &str) -> Option<String> {
    let rest = error.strip_prefix("invalid type: ")?;
    let (actual, rest) = rest.split_once(", expected ")?;
    let actual_kind = kind_of_serde_name(actual.split_whitespace().next()?)?;
    let expected = rest.split(" at line ").next().unwrap_or(rest).trim();
    let Value::Object(map) = serde_json::from_str::<Value>(arguments).ok()? else {
        return None;
    };
    let candidates: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(_, value)| kind_of_value(value) == actual_kind)
        .collect();
    let mut out = match candidates.as_slice() {
        [] => return None,
        [(key, _)] => format!(
            "The value of `{key}` is {} {actual_kind}, but {expected} is expected.",
            article(actual_kind)
        ),
        many => {
            let names: Vec<String> = many.iter().map(|(key, _)| format!("`{key}`")).collect();
            format!(
                "One of {} is {} {actual_kind}, but {expected} is expected.",
                names.join(", "),
                article(actual_kind)
            )
        }
    };
    // `{"message": {"message": "..."}}`: the value was wrapped in an object of the same name.
    for (key, value) in &candidates {
        if let Value::Object(inner) = value
            && inner.contains_key(key.as_str())
        {
            out.push_str(&format!(
                " `{key}` holds an object that also has `{key}`: pass the value directly, as {{\"{key}\": ...}}, not nested."
            ));
        }
    }
    Some(out)
}

fn article(kind: &str) -> &'static str {
    if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    }
}

#[cfg(test)]
mod tests {
    use super::describe_type_error;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_nested_value_names_the_field_and_says_not_to_nest() {
        let text = describe_type_error(
            r#"{"message":{"message":"x"},"fork_context":false}"#,
            "invalid type: map, expected a string at line 1 column 12",
        )
        .expect("described");
        assert!(text.starts_with("The value of `message` is an object, but a string is expected."));
        assert!(text.contains("not nested"));
    }

    #[test]
    fn several_candidates_are_all_named() {
        let text = describe_type_error(
            r#"{"a":[1],"b":[2],"c":"s"}"#,
            "invalid type: sequence, expected a string at line 1 column 8",
        )
        .expect("described");
        assert_eq!(
            text,
            "One of `a`, `b` is an array, but a string is expected."
        );
    }

    #[test]
    fn other_errors_and_non_objects_are_left_alone() {
        assert_eq!(
            describe_type_error("{}", "unknown field `x`, expected `y` at line 1 column 3"),
            None
        );
        assert_eq!(
            describe_type_error("[1]", "invalid type: sequence, expected a string"),
            None
        );
        assert_eq!(
            describe_type_error(r#"{"a":"s"}"#, "invalid type: map, expected a string"),
            None
        );
    }
}
