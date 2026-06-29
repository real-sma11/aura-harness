//! Result-shape transforms applied to trusted-provider responses.
//!
//! `apply_result_transform` interprets a [`TrustedIntegrationResultTransform`]
//! against the provider's raw JSON response and produces the canonical
//! envelope the agent sees (`{ key: ... }` for `WrapPointer` /
//! `ProjectArray` / `ProjectObject`, `{ query, results, more_results_available }`
//! for `BraveSearch`).

use super::super::json_paths::required_string;
use super::{TrustedIntegrationResultField, TrustedIntegrationResultTransform};
use crate::error::ToolError;
use serde_json::{json, Value};

pub(super) fn apply_result_transform(
    response: &Value,
    transform: &TrustedIntegrationResultTransform,
    args: &Value,
) -> Result<Value, ToolError> {
    match transform {
        TrustedIntegrationResultTransform::WrapPointer { key, pointer } => Ok(object_with_entry(
            key,
            response
                .pointer(pointer)
                .cloned()
                .unwrap_or_else(|| json!({})),
        )),
        TrustedIntegrationResultTransform::ProjectArray {
            key,
            pointer,
            fields,
            extras,
        } => {
            let source = pointer
                .as_deref()
                .map_or(Some(response), |path| response.pointer(path));
            let items = source
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|item| project_fields(&item, fields))
                .collect::<Vec<_>>();
            let mut result = object_with_entry(key, Value::Array(items));
            for extra in extras {
                let value = response
                    .pointer(&extra.pointer)
                    .cloned()
                    .or_else(|| extra.default_value.clone())
                    .unwrap_or(Value::Null);
                result[&extra.output] = value;
            }
            Ok(result)
        }
        TrustedIntegrationResultTransform::ProjectObject {
            key,
            pointer,
            fields,
        } => {
            let source = pointer
                .as_deref()
                .map_or(Some(response), |path| response.pointer(path))
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(object_with_entry(key, project_fields(&source, fields)))
        }
        TrustedIntegrationResultTransform::BraveSearch { vertical } => {
            brave_search_results(response, vertical, args)
        }
    }
}

/// Shape Brave Search responses into the canonical
/// `{ query, results: [...], more_results_available }` envelope.
///
/// Used both by the in-line `brave_search` runtime path and by the
/// declarative `apply_result_transform::BraveSearch` transform — keeping
/// one implementation guarantees both code paths produce bit-identical
/// output for the same Brave response.
pub(super) fn brave_search_results(
    response: &Value,
    vertical: &str,
    args: &Value,
) -> Result<Value, ToolError> {
    let query = required_string(args, &["query", "q"])?;
    // Brave nests *web* results under `web.results`, but the News Search API
    // returns its results at the top-level `results` array (envelope
    // `type: "news"`, with no `news` wrapper object). Use a vertical-aware
    // pointer — `/{vertical}/results` is correct for web, but news would
    // silently parse as empty under that path.
    let results_pointer = if vertical == "news" {
        "/results".to_string()
    } else {
        format!("/{vertical}/results")
    };
    let items = response
        .pointer(&results_pointer)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|item| {
            json!({
                "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                "url": item
                    .get("url")
                    .or_else(|| item.get("profile"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "description": item
                    .get("description")
                    .or_else(|| item.get("snippet"))
                    .and_then(Value::as_str),
                "age": item.get("age").and_then(Value::as_str),
                "source": item.get("source").and_then(Value::as_str),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "query": query,
        "results": items,
        "more_results_available": response
            .pointer("/query/more_results_available")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

fn object_with_entry(key: &str, value: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), value);
    Value::Object(map)
}

fn project_fields(source: &Value, fields: &[TrustedIntegrationResultField]) -> Value {
    let mut result = json!({});
    for field in fields {
        result[&field.output] = source
            .pointer(&field.pointer)
            .cloned()
            .unwrap_or(Value::Null);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::brave_search_results;
    use serde_json::json;

    #[test]
    fn web_results_parse_from_nested_web_object() {
        // Brave web responses nest results under `web.results`.
        let response = json!({
            "web": {
                "results": [
                    { "title": "AURA", "url": "https://aura.ai", "description": "desc", "age": "1 day" }
                ]
            },
            "query": { "more_results_available": true }
        });
        let out = brave_search_results(&response, "web", &json!({ "query": "aura os" })).unwrap();
        assert_eq!(out["query"], "aura os");
        assert_eq!(out["more_results_available"], true);
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["title"], "AURA");
        assert_eq!(results[0]["url"], "https://aura.ai");
    }

    #[test]
    fn news_results_parse_from_top_level_results() {
        // Regression guard: Brave's News API returns results at the TOP-LEVEL
        // `results` array (no `news` wrapper). Before the vertical-aware
        // pointer fix, the news vertical looked up `/news/results`, found
        // nothing, and silently returned an empty list.
        let response = json!({
            "type": "news",
            "results": [
                { "title": "Headline", "url": "https://news.example/x", "description": "snippet", "age": "2 hours", "source": "Example" }
            ],
            "query": { "more_results_available": false }
        });
        let out = brave_search_results(&response, "news", &json!({ "query": "aura os" })).unwrap();
        let results = out["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            1,
            "news results must parse from top-level `results`"
        );
        assert_eq!(results[0]["title"], "Headline");
        assert_eq!(results[0]["url"], "https://news.example/x");
        assert_eq!(results[0]["source"], "Example");
        assert_eq!(out["more_results_available"], false);
    }
}
