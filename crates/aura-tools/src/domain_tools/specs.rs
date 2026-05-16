//! Spec domain tool handlers.

use serde_json::{json, Value};
use tracing::debug;

use super::api::{DomainApi, SpecDescriptor};
use super::helpers::{domain_err, domain_ok, require_str, str_field};

/// Maximum bytes of `markdown_contents` returned by `get_spec` before the body
/// is truncated and a marker appended. Mirrors the `fs_read` cap so a single
/// tool result can never exceed the upstream proxy's per-message envelope.
const MAX_SPEC_MARKDOWN_BYTES: usize = 64 * 1024;

/// Find the largest byte index `<= max_bytes` that lies on a UTF-8 char
/// boundary of `s`. Mirrors the pattern used by `fs_read` for the
/// line-sliced render path.
fn truncate_on_utf8_boundary(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// Standard marker appended to a truncated `markdown_contents` so the LLM
/// gets a consistent, machine-readable hint about how to recover the missing
/// bytes (or to stop re-reading the full body each turn).
fn spec_truncation_marker(dropped: usize, total: usize) -> String {
    format!(
        "\n... [truncated {dropped} of {total} bytes; fetch the spec via a slice/paginated tool or read fewer fields if you need the full body.]"
    )
}

/// Build the LLM-facing JSON envelope for a single spec, applying the 64 KB
/// cap on `markdown_contents` and surfacing `truncated_markdown` /
/// `total_markdown_bytes` siblings when the body is truncated. All other
/// fields are passed through unchanged.
fn build_spec_json(s: &SpecDescriptor) -> Value {
    let total = s.content.len();
    let kept = truncate_on_utf8_boundary(&s.content, MAX_SPEC_MARKDOWN_BYTES);
    let truncated = kept < total;
    let body = if truncated {
        let mut out = String::with_capacity(kept + 128);
        out.push_str(&s.content[..kept]);
        out.push_str(&spec_truncation_marker(total - kept, total));
        out
    } else {
        s.content.clone()
    };
    let mut spec = json!({
        "id": s.id,
        "project_id": s.project_id,
        "title": s.title,
        "markdown_contents": body,
        "order": s.order,
        "parent_id": s.parent_id,
    });
    if truncated {
        spec["truncated_markdown"] = json!(true);
        spec["total_markdown_bytes"] = json!(total);
    }
    spec
}

pub async fn list_specs(api: &dyn DomainApi, project_id: &str, input: &Value) -> String {
    debug!(project_id, "domain_tools: list_specs");
    let jwt = str_field(input, "jwt");
    match api.list_specs(project_id, jwt.as_deref()).await {
        Ok(specs) => {
            let summaries: Vec<Value> = specs
                .iter()
                .map(|s| {
                    json!({
                        "spec_id": s.id,
                        "title": s.title,
                        "order": s.order,
                        "markdown_bytes": s.content.len(),
                    })
                })
                .collect();
            domain_ok(json!({ "specs": summaries }))
        }
        Err(e) => domain_err(e),
    }
}

pub async fn get_spec(api: &dyn DomainApi, project_id: &str, input: &Value) -> String {
    debug!(project_id, "domain_tools: get_spec");
    let spec_id = match require_str(input, "spec_id") {
        Ok(id) => id,
        Err(e) => return domain_err(&e),
    };
    let jwt = str_field(input, "jwt");
    match api.get_spec(&spec_id, jwt.as_deref()).await {
        Ok(s) => domain_ok(json!({ "spec": build_spec_json(&s) })),
        Err(e) => domain_err(e),
    }
}

pub async fn create_spec(api: &dyn DomainApi, project_id: &str, input: &Value) -> String {
    debug!(project_id, "domain_tools: create_spec");
    let title = str_field(input, "title").unwrap_or_default();
    let content = str_field(input, "markdown_contents")
        .or_else(|| str_field(input, "content"))
        .unwrap_or_default();
    let jwt = str_field(input, "jwt");

    // Auto-derive orderIndex from existing spec count so specs are
    // numbered in creation order without the caller needing to track it.
    let order = match api.list_specs(project_id, jwt.as_deref()).await {
        #[allow(clippy::cast_possible_truncation)]
        Ok(specs) => specs.len() as u32,
        Err(_) => 0,
    };

    match api
        .create_spec(project_id, &title, &content, order, jwt.as_deref())
        .await
    {
        Ok(s) => domain_ok(json!({ "spec": s })),
        Err(e) => domain_err(e),
    }
}

pub async fn update_spec(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: update_spec");
    let spec_id = match require_str(input, "spec_id") {
        Ok(id) => id,
        Err(e) => return domain_err(&e),
    };
    let title = str_field(input, "title");
    let content = str_field(input, "markdown_contents").or_else(|| str_field(input, "content"));
    let jwt = str_field(input, "jwt");

    match api
        .update_spec(
            &spec_id,
            title.as_deref(),
            content.as_deref(),
            jwt.as_deref(),
        )
        .await
    {
        Ok(s) => domain_ok(json!({ "spec": s })),
        Err(e) => domain_err(e),
    }
}

pub async fn delete_spec(api: &dyn DomainApi, _project_id: &str, input: &Value) -> String {
    debug!("domain_tools: delete_spec");
    let spec_id = match require_str(input, "spec_id") {
        Ok(id) => id,
        Err(e) => return domain_err(&e),
    };
    let jwt = str_field(input, "jwt");
    match api.delete_spec(&spec_id, jwt.as_deref()).await {
        Ok(()) => domain_ok(json!({ "deleted": spec_id })),
        Err(e) => domain_err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_tools::api::{
        CreateSessionParams, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
        SaveMessageParams, SessionDescriptor, TaskDescriptor, TaskUpdate,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Minimal `DomainApi` test double specialised for the spec handlers.
    /// Only the methods exercised by these tests are populated; everything
    /// else bails so a misrouted call shows up loudly.
    #[derive(Default)]
    struct SpecMockApi {
        specs_to_return: Mutex<Vec<SpecDescriptor>>,
        single_spec: Mutex<Option<SpecDescriptor>>,
    }

    impl SpecMockApi {
        fn with_single_spec(spec: SpecDescriptor) -> Self {
            Self {
                specs_to_return: Mutex::new(vec![]),
                single_spec: Mutex::new(Some(spec)),
            }
        }
        fn with_listing(specs: Vec<SpecDescriptor>) -> Self {
            Self {
                specs_to_return: Mutex::new(specs),
                single_spec: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl DomainApi for SpecMockApi {
        async fn list_specs(
            &self,
            _project_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<Vec<SpecDescriptor>> {
            Ok(self.specs_to_return.lock().unwrap().clone())
        }
        async fn get_spec(
            &self,
            spec_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            self.single_spec
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no spec configured for id {spec_id}"))
        }
        async fn create_spec(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: u32,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_spec(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            anyhow::bail!("unused")
        }
        async fn delete_spec(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn list_tasks(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<TaskDescriptor>> {
            Ok(vec![])
        }
        async fn create_task(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: &[String],
            _: u32,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_task(
            &self,
            _: &str,
            _: TaskUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn delete_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            anyhow::bail!("unused")
        }
        async fn transition_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn claim_next_task(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<Option<TaskDescriptor>> {
            Ok(None)
        }
        async fn get_task(&self, _: &str, _: Option<&str>) -> anyhow::Result<TaskDescriptor> {
            anyhow::bail!("unused")
        }
        async fn get_project(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            anyhow::bail!("unused")
        }
        async fn update_project(
            &self,
            _: &str,
            _: ProjectUpdate,
            _: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            anyhow::bail!("unused")
        }
        async fn create_log(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        async fn list_logs(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<u64>,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!([]))
        }
        async fn get_project_stats(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
        async fn list_messages(
            &self,
            _: &str,
            _: &str,
        ) -> anyhow::Result<Vec<MessageDescriptor>> {
            Ok(vec![])
        }
        async fn save_message(&self, _: SaveMessageParams) -> anyhow::Result<()> {
            Ok(())
        }
        async fn create_session(
            &self,
            _: CreateSessionParams,
        ) -> anyhow::Result<SessionDescriptor> {
            anyhow::bail!("unused")
        }
        async fn get_active_session(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<SessionDescriptor>> {
            Ok(None)
        }
        async fn orbit_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok("{}".into())
        }
        async fn network_api_call(
            &self,
            _: &str,
            _: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok("{}".into())
        }
    }

    fn make_spec(id: &str, content: String) -> SpecDescriptor {
        SpecDescriptor {
            id: id.into(),
            project_id: "p1".into(),
            title: format!("spec {id}"),
            content,
            order: 0,
            parent_id: None,
        }
    }

    fn parse_envelope(raw: &str) -> Value {
        serde_json::from_str::<Value>(raw).expect("tool result must be JSON")
    }

    #[tokio::test]
    async fn test_get_spec_truncates_large_markdown() {
        let total_bytes = 200 * 1024;
        let big = "a".repeat(total_bytes);
        let api = SpecMockApi::with_single_spec(make_spec("s1", big));
        let input = json!({ "spec_id": "s1" });

        let raw = get_spec(&api, "p1", &input).await;
        let env = parse_envelope(&raw);
        assert_eq!(env["ok"], json!(true), "envelope must report ok");
        let spec = &env["spec"];

        let body = spec["markdown_contents"]
            .as_str()
            .expect("markdown_contents must be present as a string");
        let marker = spec_truncation_marker(total_bytes - MAX_SPEC_MARKDOWN_BYTES, total_bytes);
        let expected_len = MAX_SPEC_MARKDOWN_BYTES + marker.len();
        assert_eq!(
            body.len(),
            expected_len,
            "body must be exactly MAX_SPEC_MARKDOWN_BYTES plus the truncation marker"
        );
        assert!(body.len() <= MAX_SPEC_MARKDOWN_BYTES + marker.len());
        assert!(
            body.contains("[truncated"),
            "body must carry the truncation marker substring"
        );
        assert_eq!(
            spec["truncated_markdown"],
            json!(true),
            "structured truncation flag must be set"
        );
        assert_eq!(
            spec["total_markdown_bytes"],
            json!(total_bytes),
            "total_markdown_bytes must report the original (pre-truncation) size"
        );
        assert_eq!(
            spec["id"], json!("s1"),
            "non-markdown fields must pass through unchanged"
        );
        assert_eq!(spec["title"], json!("spec s1"));
    }

    #[tokio::test]
    async fn test_get_spec_passes_small_specs_through_unchanged() {
        let body = "x".repeat(1024);
        let api = SpecMockApi::with_single_spec(make_spec("s2", body.clone()));
        let input = json!({ "spec_id": "s2" });

        let raw = get_spec(&api, "p1", &input).await;
        let env = parse_envelope(&raw);
        let spec = &env["spec"];

        assert_eq!(
            spec["markdown_contents"].as_str(),
            Some(body.as_str()),
            "small bodies must pass through byte-for-byte"
        );
        assert!(
            spec.get("truncated_markdown").is_none()
                || spec["truncated_markdown"] == json!(false),
            "truncated_markdown must be absent (or false) when the body fits"
        );
        assert!(
            spec.get("total_markdown_bytes").is_none(),
            "total_markdown_bytes is only added when the body was truncated"
        );
    }

    #[tokio::test]
    async fn test_list_specs_strips_markdown_contents() {
        let body_a = "a".repeat(4096);
        let body_b = "b".repeat(12345);
        let api = SpecMockApi::with_listing(vec![
            make_spec("s1", body_a.clone()),
            make_spec("s2", body_b.clone()),
        ]);

        let raw = list_specs(&api, "p1", &json!({})).await;
        let env = parse_envelope(&raw);
        let listing = env["specs"]
            .as_array()
            .expect("specs envelope must contain an array");
        assert_eq!(listing.len(), 2, "every input spec must be represented");

        let entry_a = &listing[0];
        assert!(
            entry_a.get("markdown_contents").is_none(),
            "list_specs entries must never ship markdown_contents"
        );
        assert!(
            entry_a.get("content").is_none(),
            "list_specs entries must not leak the raw content field either"
        );
        assert_eq!(
            entry_a["spec_id"], json!("s1"),
            "id metadata must round-trip"
        );
        assert_eq!(entry_a["title"], json!("spec s1"));
        assert_eq!(
            entry_a["markdown_bytes"],
            json!(body_a.len()),
            "markdown_bytes must mirror the input length"
        );

        let entry_b = &listing[1];
        assert!(entry_b.get("markdown_contents").is_none());
        assert_eq!(entry_b["spec_id"], json!("s2"));
        assert_eq!(entry_b["markdown_bytes"], json!(body_b.len()));
    }
}
