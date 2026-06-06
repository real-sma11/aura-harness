//! Small JSON-path / argument-extraction helpers shared by the resolver
//! submodules.
//!
//! Split out of `resolver.rs` in Wave 6 / T4 so `trusted.rs` and
//! `installed.rs` can pull in these utilities without pulling in each
//! other. The only non-trivial function here is `insert_json_path`, which
//! Wave 3 previously hardened to never `.expect()` — the structure must
//! stay intact.

use crate::error::ToolError;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Insert `value` into `target` following a dot-separated `path`.
///
/// Wave 3 removed the `.expect()` that used to live on the final
/// `.as_object_mut()` — the pattern match is load-bearing. Do not
/// collapse it back into an `.expect()` "because it can't happen":
/// if a caller ever passes a root non-object value the `else` branch
/// is the only thing that keeps us panic-free (rules §4.1).
pub(super) fn insert_json_path(
    target: &mut Value,
    path: &str,
    value: Value,
) -> Result<(), ToolError> {
    let parts = path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return Err(ToolError::ExternalToolError(
            "trusted integration metadata declared an empty target path".into(),
        ));
    }

    let mut current = target;
    for part in &parts[..parts.len() - 1] {
        if !current.is_object() {
            *current = json!({});
        }
        // `current` was just rewritten to an empty object above if it
        // wasn't one, so this match cannot take the `None` branch in
        // practice. The pattern match (rather than `.expect()`) keeps us
        // panic-free per rules §4.1 and surfaces a structured error if
        // the invariant ever changes.
        let Some(map) = current.as_object_mut() else {
            return Err(ToolError::ExternalToolError(format!(
                "trusted integration target path `{path}` segment `{part}` does not resolve to an object"
            )));
        };
        current = map.entry((*part).to_string()).or_insert_with(|| json!({}));
    }

    current
        .as_object_mut()
        .ok_or_else(|| {
            ToolError::ExternalToolError(format!(
                "trusted integration target path `{path}` does not resolve to an object"
            ))
        })?
        .insert(parts[parts.len() - 1].to_string(), value);
    Ok(())
}

pub(super) fn required_string(args: &Value, keys: &[&str]) -> Result<String, ToolError> {
    optional_string(args, keys).ok_or_else(|| {
        ToolError::ExternalToolError(format!("missing required field `{}`", keys[0]))
    })
}

pub(super) fn optional_string(args: &Value, keys: &[&str]) -> Option<String> {
    optional_string_from_names(
        args,
        &keys
            .iter()
            .map(|key| (*key).to_string())
            .collect::<Vec<_>>(),
    )
}

pub(super) fn optional_string_from_names(args: &Value, keys: &[String]) -> Option<String> {
    keys.iter().find_map(|key| {
        args.get(key.as_str())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub(super) fn optional_string_from_names_map(
    values: &HashMap<String, Value>,
    keys: &[String],
) -> Option<String> {
    keys.iter().find_map(|key| {
        values
            .get(key.as_str())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub(super) fn required_string_list(args: &Value, keys: &[&str]) -> Result<Vec<String>, ToolError> {
    optional_string_list(args, keys).ok_or_else(|| {
        ToolError::ExternalToolError(format!("missing required field `{}`", keys[0]))
    })
}

pub(super) fn optional_string_list(args: &Value, keys: &[&str]) -> Option<Vec<String>> {
    optional_string_list_from_names(
        args,
        &keys
            .iter()
            .map(|key| (*key).to_string())
            .collect::<Vec<_>>(),
    )
}

pub(super) fn optional_string_list_from_names(
    args: &Value,
    keys: &[String],
) -> Option<Vec<String>> {
    keys.iter().find_map(|key| {
        let value = args.get(key.as_str())?;
        if let Some(single) = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(vec![single.to_string()]);
        }
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|items| !items.is_empty())
    })
}

pub(super) fn optional_string_list_from_names_map(
    values: &HashMap<String, Value>,
    keys: &[String],
) -> Option<Vec<String>> {
    keys.iter().find_map(|key| {
        let value = values.get(key.as_str())?;
        if let Some(single) = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(vec![single.to_string()]);
        }
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|items| !items.is_empty())
    })
}

pub(super) fn optional_positive_number(args: &Value, keys: &[&str]) -> Option<u64> {
    optional_positive_number_from_names(
        args,
        &keys
            .iter()
            .map(|key| (*key).to_string())
            .collect::<Vec<_>>(),
    )
}

pub(super) fn optional_positive_number_from_names(args: &Value, keys: &[String]) -> Option<u64> {
    keys.iter()
        .find_map(|key| args.get(key.as_str()).and_then(Value::as_u64))
}

pub(super) fn optional_positive_number_from_names_map(
    values: &HashMap<String, Value>,
    keys: &[String],
) -> Option<u64> {
    keys.iter()
        .find_map(|key| values.get(key.as_str()).and_then(Value::as_u64))
}

pub(super) fn optional_boolean_from_names(args: &Value, keys: &[String]) -> Option<bool> {
    keys.iter()
        .find_map(|key| args.get(key.as_str()).and_then(Value::as_bool))
}

pub(super) fn optional_boolean_from_names_map(
    values: &HashMap<String, Value>,
    keys: &[String],
) -> Option<bool> {
    keys.iter()
        .find_map(|key| values.get(key.as_str()).and_then(Value::as_bool))
}

pub(super) fn optional_json_from_names(args: &Value, keys: &[String]) -> Option<Value> {
    keys.iter().find_map(|key| args.get(key.as_str()).cloned())
}

pub(super) fn optional_json_from_names_map(
    values: &HashMap<String, Value>,
    keys: &[String],
) -> Option<Value> {
    keys.iter()
        .find_map(|key| values.get(key.as_str()).cloned())
}

pub(super) fn ensure_slack_ok(response: &Value) -> Result<(), ToolError> {
    if response.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(());
    }
    let error = response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown slack error");
    Err(ToolError::ExternalToolError(format!(
        "slack api error: {error}"
    )))
}
