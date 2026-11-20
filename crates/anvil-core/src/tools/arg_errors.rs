//! Explains tool arguments that do not fit the tool's schema.
//!
//! serde's own message names a missing field but not a field of the wrong
//! type ("invalid type: string \"5\", expected usize"), and a model cannot
//! fix what it cannot find. This reads the same JSON schema the model was
//! given and names each argument, what was wrong with it, and what it
//! accepts.

use serde_json::{Map, Value};

/// Most characters of a string value quoted back.
const MAX_QUOTED_CHARS: usize = 40;

/// Builds the error for arguments that failed to parse.
///
/// # Arguments
///
/// * `args` - The arguments the model sent.
/// * `schema` - The tool's JSON schema.
/// * `fallback` - serde's message, used when the schema finds nothing wrong.
///
/// # Returns
///
/// A message that starts with `invalid arguments:`.
pub(crate) fn explain(args: &Value, schema: &Value, fallback: &str) -> String {
    let mut problems = Vec::new();
    match args.as_object() {
        Some(object) => check_object(object, schema, "", &mut problems),
        None => problems.push(format!(
            "the arguments are {}; they must be a JSON object",
            describe(args)
        )),
    }
    if problems.is_empty() {
        problems.push(fallback.to_owned());
    }
    let mut message = format!("invalid arguments: {}.", problems.join("; "));
    let accepted = accepted(schema);
    if !accepted.is_empty() {
        message.push_str(&format!(" Accepted arguments: {accepted}."));
    }
    message
}

/// Checks one object against its schema, adding a line per problem.
fn check_object(object: &Map<String, Value>, schema: &Value, prefix: &str, out: &mut Vec<String>) {
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for name in required(schema) {
        if object.get(name).is_none_or(Value::is_null) {
            let expected = properties.get(name).map_or_else(String::new, expected);
            out.push(format!(
                "missing argument `{prefix}{name}`: {expected} is required"
            ));
        }
    }
    for (name, value) in object {
        let shown = format!("{prefix}{name}");
        let Some(property) = properties.get(name) else {
            out.push(format!("unknown argument `{shown}`"));
            continue;
        };
        if value.is_null() && required(schema).any(|required| required == name) {
            continue;
        }
        if !fits(value, property) {
            out.push(format!(
                "argument `{shown}` is {}; it must be {}",
                describe(value),
                expected(property)
            ));
            continue;
        }
        check_nested(value, property, &shown, out);
    }
}

/// Checks the objects inside an array or object argument.
fn check_nested(value: &Value, property: &Value, shown: &str, out: &mut Vec<String>) {
    match value {
        Value::Object(inner) if property.get("properties").is_some() => {
            check_object(inner, property, &format!("{shown}."), out);
        }
        Value::Array(items) => {
            let Some(item_schema) = property.get("items") else {
                return;
            };
            for (index, item) in items.iter().enumerate() {
                let item_shown = format!("{shown}[{index}]");
                if !fits(item, item_schema) {
                    out.push(format!(
                        "argument `{item_shown}` is {}; it must be {}",
                        describe(item),
                        expected(item_schema)
                    ));
                } else {
                    check_nested(item, item_schema, &item_shown, out);
                }
            }
        }
        _ => {}
    }
}

/// The names a schema lists as required.
fn required(schema: &Value) -> impl Iterator<Item = &str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// The JSON types a property allows; empty when the schema does not say.
fn types(property: &Value) -> Vec<&str> {
    match property.get("type") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether a value fits a property's type, minimum and allowed values.
fn fits(value: &Value, property: &Value) -> bool {
    if let Some(allowed) = property.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return false;
    }
    let types = types(property);
    if types.is_empty() {
        return true;
    }
    types.iter().any(|kind| match *kind {
        "null" => value.is_null(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => {
            let whole = value.is_i64() || value.is_u64();
            let minimum = property.get("minimum").and_then(Value::as_f64);
            whole && minimum.is_none_or(|minimum| value.as_f64().is_some_and(|v| v >= minimum))
        }
        _ => true,
    })
}

/// What a property accepts, in words.
fn expected(property: &Value) -> String {
    if let Some(allowed) = property.get("enum").and_then(Value::as_array) {
        let names: Vec<String> = allowed.iter().map(|value| format!("`{value}`")).collect();
        return format!("one of {}", names.join(", "));
    }
    let words: Vec<String> = types(property)
        .into_iter()
        .filter(|kind| *kind != "null")
        .map(|kind| match kind {
            "string" => "a string".to_owned(),
            "boolean" => "true or false".to_owned(),
            "object" => "an object".to_owned(),
            "number" => "a number".to_owned(),
            "integer" => match property.get("minimum").and_then(Value::as_f64) {
                Some(minimum) => format!("a whole number of {minimum} or more"),
                None => "a whole number".to_owned(),
            },
            "array" => {
                let item = property.get("items").map(types).unwrap_or_default();
                if item.contains(&"object") {
                    "an array of objects".to_owned()
                } else {
                    "an array".to_owned()
                }
            }
            other => other.to_owned(),
        })
        .collect();
    if words.is_empty() {
        "a value".to_owned()
    } else {
        words.join(" or ")
    }
}

/// A value, in words, as the model sent it.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => format!("the boolean {flag}"),
        Value::Number(number) => format!("the number {number}"),
        Value::String(text) => match text.char_indices().nth(MAX_QUOTED_CHARS) {
            Some((at, _)) => format!("the string \"{}...\"", &text[..at]),
            None => format!("the string \"{text}\""),
        },
        Value::Array(_) => "an array".to_owned(),
        Value::Object(_) => "an object".to_owned(),
    }
}

/// The top-level arguments, each with its type and whether it is required.
fn accepted(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: Vec<&str> = required(schema).collect();
    properties
        .iter()
        .map(|(name, property)| {
            let need = if required.contains(&name.as_str()) {
                "required"
            } else {
                "optional"
            };
            format!("`{name}` ({}, {need})", expected(property))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": ["integer", "null"], "minimum": 0},
                "files": {"type": ["array", "null"], "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "limit": {"type": ["integer", "null"], "minimum": 0}},
                    "required": ["path"]
                }},
                "mode": {"enum": ["a", "b"]}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn a_wrong_type_names_the_argument_and_what_it_accepts() {
        let message = explain(&json!({"path": "x", "offset": "5"}), &schema(), "serde");
        assert!(
            message.contains(
                "argument `offset` is the string \"5\"; it must be a whole number of 0 or more"
            ),
            "{message}"
        );
        assert!(message.contains("`path` (a string, required)"), "{message}");
    }

    #[test]
    fn a_missing_negative_nested_or_unknown_argument_is_named() {
        let message = explain(
            &json!({"file_path": "x", "files": [{"path": "a", "limit": -1}, {"limit": 2}]}),
            &schema(),
            "serde",
        );
        assert!(
            message.contains("missing argument `path`: a string is required"),
            "{message}"
        );
        assert!(
            message.contains("unknown argument `file_path`"),
            "{message}"
        );
        assert!(
            message.contains("argument `files[0].limit` is the number -1"),
            "{message}"
        );
        assert!(
            message.contains("missing argument `files[1].path`"),
            "{message}"
        );
    }

    #[test]
    fn non_objects_enums_and_the_fallback_are_explained() {
        let message = explain(&json!([1]), &schema(), "serde");
        assert!(message.contains("the arguments are an array; they must be a JSON object"));
        let message = explain(&json!({"path": "x", "mode": "c"}), &schema(), "serde");
        assert!(
            message.contains("argument `mode` is the string \"c\"; it must be one of `\"a\"`"),
            "{message}"
        );
        let message = explain(&json!({"path": "x"}), &schema(), "serde says why");
        assert!(message.starts_with("invalid arguments: serde says why."));
    }
}
