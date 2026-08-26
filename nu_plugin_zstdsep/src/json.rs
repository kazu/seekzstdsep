//! JSON, parsed inside the plugin.
//!
//! The plugin would rather resolve `from <format>` in the caller's scope and know no format at
//! all, and for a command it does. A cell path cannot: nushell answers `FindDecl` with "attempted
//! to call FindDecl outside of a command invocation", because it services custom value ops with no
//! execution context (`nu-plugin-engine`, `custom_value_op_expecting_value` passes `None`).
//!
//! So `$h.10` has to parse in process or hand back a string, and one format parsed beats none.
//! json, jsonl and ndjson are the same thing once records are separated: one JSON value per
//! record.
use nu_protocol::{FromValue, ShellError, Span, Value, record, shell_error::generic::GenericError};

/// One record's JSON as a value.
pub fn parse(text: &str, span: Span) -> Result<Value, ShellError> {
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        ShellError::Generic(
            GenericError::new("the record is not JSON", e.to_string(), span)
                .with_help("pass --raw to get records as strings, or --format to name another one"),
        )
    })?;
    Ok(convert(parsed, span))
}

fn convert(value: serde_json::Value, span: Span) -> Value {
    match value {
        serde_json::Value::Null => Value::nothing(span),
        serde_json::Value::Bool(b) => Value::bool(b, span),
        serde_json::Value::Number(n) => number(n, span),
        serde_json::Value::String(s) => Value::string(s, span),
        serde_json::Value::Array(items) => {
            Value::list(items.into_iter().map(|v| convert(v, span)).collect(), span)
        }
        serde_json::Value::Object(fields) => {
            // Column order is the order the keys were written, which is what makes a table of
            // these readable. `serde_json`'s preserve_order feature is what keeps it.
            let mut out = record!();
            for (key, value) in fields {
                out.push(key, convert(value, span));
            }
            Value::record(out, span)
        }
    }
}

/// Integers stay integers where they fit, since that is what a cell path is usually compared
/// against. What does not fit is a float, as it would be in `from json`.
fn number(n: serde_json::Number, span: Span) -> Value {
    if let Some(i) = n.as_i64() {
        Value::int(i, span)
    } else if let Some(f) = n.as_f64() {
        Value::float(f, span)
    } else {
        Value::string(n.to_string(), span)
    }
}

/// One value as the JSON text of one record, the way `to json --raw` writes it.
///
/// The conversion is nushell's own (`nu-json`), so a record written here and one written by
/// `to json` differ in nothing but the indentation this leaves out.
pub fn text(value: Value, span: Span) -> Result<String, ShellError> {
    let json = nu_json::Value::from_value(value)?;
    nu_json::to_string_raw(&json).map_err(|e| {
        ShellError::Generic(GenericError::new(
            "the record cannot be written as JSON",
            e.to_string(),
            span,
        ))
    })
}
