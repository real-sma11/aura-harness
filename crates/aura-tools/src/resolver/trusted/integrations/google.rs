//! Google trusted-integration handlers for Gmail write actions and
//! Google Calendar mutations.

use super::super::super::json_paths::{
    optional_string, optional_string_list, required_string, required_string_list,
};
use super::super::{build_runtime_url, ToolResolver, TrustedIntegrationArgBinding};
use crate::error::ToolError;
use aura_core_types::{InstalledToolRuntimeIntegration, InstalledToolRuntimeProviderExecution};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Method;
use serde_json::{json, Value};

impl ToolResolver {
    pub(in super::super) async fn gmail_send_email(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let message = gmail_message_from_args(args, "gmail_send_email")?;
        let response = self
            .provider_json_request(
                Method::POST,
                &format!(
                    "{}/gmail/v1/users/me/messages/send",
                    google_base_url(provider, integration)
                ),
                provider,
                integration,
                Some(message),
            )
            .await?;
        Ok(gmail_message_result(&response))
    }

    pub(in super::super) async fn gmail_create_draft(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let message = gmail_message_from_args(args, "gmail_create_draft")?;
        let response = self
            .provider_json_request(
                Method::POST,
                &format!(
                    "{}/gmail/v1/users/me/drafts",
                    google_base_url(provider, integration)
                ),
                provider,
                integration,
                Some(json!({ "message": message })),
            )
            .await?;
        Ok(json!({
            "draft": {
                "id": response.get("id").and_then(Value::as_str).unwrap_or_default(),
                "message": {
                    "id": response.pointer("/message/id").and_then(Value::as_str).unwrap_or_default(),
                    "thread_id": response.pointer("/message/threadId").and_then(Value::as_str).unwrap_or_default(),
                }
            }
        }))
    }

    pub(in super::super) async fn google_calendar_create_event(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let event =
            build_google_calendar_event_resource(args, true, "google_calendar_create_event")?;
        let mut query = vec![query_binding(
            &["send_updates", "sendUpdates"],
            "sendUpdates",
        )];
        if optional_bool(args, &["create_google_meet", "createGoogleMeet"]).unwrap_or(false) {
            query.push(default_query_binding("conferenceDataVersion", json!("1")));
        }
        let url = build_runtime_url(
            provider,
            integration,
            "/calendar/v3/calendars/{calendar_id}/events",
            &query,
            args,
        )?;
        let response = self
            .provider_json_request(Method::POST, &url, provider, integration, Some(event))
            .await?;
        Ok(google_calendar_event_result(&response))
    }

    pub(in super::super) async fn google_calendar_update_event(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let event =
            build_google_calendar_event_resource(args, false, "google_calendar_update_event")?;
        let mut query = vec![query_binding(
            &["send_updates", "sendUpdates"],
            "sendUpdates",
        )];
        if optional_bool(args, &["create_google_meet", "createGoogleMeet"]).unwrap_or(false) {
            query.push(default_query_binding("conferenceDataVersion", json!("1")));
        }
        let url = build_runtime_url(
            provider,
            integration,
            "/calendar/v3/calendars/{calendar_id}/events/{event_id}",
            &query,
            args,
        )?;
        let response = self
            .provider_json_request(Method::PATCH, &url, provider, integration, Some(event))
            .await?;
        Ok(google_calendar_event_result(&response))
    }

    pub(in super::super) async fn google_calendar_delete_event(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
    ) -> Result<Value, ToolError> {
        let event_id = required_string(args, &["event_id", "eventId"])?;
        let query = vec![query_binding(
            &["send_updates", "sendUpdates"],
            "sendUpdates",
        )];
        let url = build_runtime_url(
            provider,
            integration,
            "/calendar/v3/calendars/{calendar_id}/events/{event_id}",
            &query,
            args,
        )?;
        let _response = self
            .provider_json_request(Method::DELETE, &url, provider, integration, None)
            .await?;
        Ok(json!({
            "event": {
                "id": event_id,
                "deleted": true,
            }
        }))
    }
}

fn google_base_url(
    provider: &InstalledToolRuntimeProviderExecution,
    integration: &InstalledToolRuntimeIntegration,
) -> String {
    integration
        .base_url
        .as_deref()
        .unwrap_or(&provider.base_url)
        .trim_end_matches('/')
        .to_string()
}

fn gmail_message_from_args(args: &Value, tool_name: &str) -> Result<Value, ToolError> {
    let from = required_string(args, &["from"])?;
    let to = required_string_list(args, &["to"])?;
    let subject = required_string(args, &["subject"])?;
    let text = optional_string(args, &["text"]);
    let html = optional_string(args, &["html"]);
    if text.is_none() && html.is_none() {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} requires at least one of `text` or `html`"
        )));
    }

    let raw = build_gmail_raw_message(
        &from,
        &to,
        optional_string_list(args, &["cc"]).as_deref(),
        optional_string_list(args, &["bcc"]).as_deref(),
        &subject,
        text.as_deref(),
        html.as_deref(),
        tool_name,
    )?;
    let mut message = json!({ "raw": raw });
    if let Some(thread_id) = optional_string(args, &["thread_id", "threadId"]) {
        message["threadId"] = Value::String(thread_id);
    }
    Ok(message)
}

fn build_gmail_raw_message(
    from: &str,
    to: &[String],
    cc: Option<&[String]>,
    bcc: Option<&[String]>,
    subject: &str,
    text: Option<&str>,
    html: Option<&str>,
    tool_name: &str,
) -> Result<String, ToolError> {
    let from = safe_header_value("from", from, tool_name)?;
    let subject = safe_header_value("subject", subject, tool_name)?;
    let to_header = safe_header_list("to", to, tool_name)?;
    let cc_header = cc
        .filter(|items| !items.is_empty())
        .map(|items| safe_header_list("cc", items, tool_name))
        .transpose()?;
    let bcc_header = bcc
        .filter(|items| !items.is_empty())
        .map(|items| safe_header_list("bcc", items, tool_name))
        .transpose()?;
    let (content_type, body) = if let Some(html) = html.filter(|value| !value.trim().is_empty()) {
        ("text/html", html)
    } else if let Some(text) = text.filter(|value| !value.trim().is_empty()) {
        ("text/plain", text)
    } else {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} requires non-empty `text` or `html`"
        )));
    };

    let mut message = String::new();
    message.push_str("MIME-Version: 1.0\r\n");
    message.push_str(&format!("From: {from}\r\n"));
    message.push_str(&format!("To: {to_header}\r\n"));
    if let Some(cc_header) = cc_header {
        message.push_str(&format!("Cc: {cc_header}\r\n"));
    }
    if let Some(bcc_header) = bcc_header {
        message.push_str(&format!("Bcc: {bcc_header}\r\n"));
    }
    message.push_str(&format!("Subject: {subject}\r\n"));
    message.push_str(&format!(
        "Content-Type: {content_type}; charset=\"UTF-8\"\r\n"
    ));
    message.push_str("Content-Transfer-Encoding: 8bit\r\n");
    message.push_str("\r\n");
    message.push_str(body);

    Ok(URL_SAFE_NO_PAD.encode(message.as_bytes()))
}

fn safe_header_list(name: &str, values: &[String], tool_name: &str) -> Result<String, ToolError> {
    if values.is_empty() {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} requires at least one `{name}` recipient"
        )));
    }
    values
        .iter()
        .map(|value| safe_header_value(name, value, tool_name))
        .collect::<Result<Vec<_>, _>>()
        .map(|values| values.join(", "))
}

fn safe_header_value(name: &str, value: &str, tool_name: &str) -> Result<String, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} `{name}` cannot be empty"
        )));
    }
    if value.contains('\r') || value.contains('\n') {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} `{name}` cannot contain newlines"
        )));
    }
    Ok(value.to_string())
}

fn gmail_message_result(response: &Value) -> Value {
    json!({
        "message": {
            "id": response.get("id").and_then(Value::as_str).unwrap_or_default(),
            "thread_id": response.get("threadId").and_then(Value::as_str).unwrap_or_default(),
            "label_ids": response.get("labelIds").cloned().unwrap_or_else(|| json!([])),
        }
    })
}

fn build_google_calendar_event_resource(
    args: &Value,
    require_summary_and_time: bool,
    tool_name: &str,
) -> Result<Value, ToolError> {
    let mut event = json!({});
    let mut inserted = false;

    if require_summary_and_time {
        event["summary"] = Value::String(required_string(args, &["summary"])?);
        event["start"] = json!({ "dateTime": required_string(args, &["start"])? });
        event["end"] = json!({ "dateTime": required_string(args, &["end"])? });
        inserted = true;
    } else {
        if let Some(summary) = optional_string(args, &["summary"]) {
            event["summary"] = Value::String(summary);
            inserted = true;
        }
        if let Some(start) = optional_string(args, &["start"]) {
            event["start"] = json!({ "dateTime": start });
            inserted = true;
        }
        if let Some(end) = optional_string(args, &["end"]) {
            event["end"] = json!({ "dateTime": end });
            inserted = true;
        }
    }

    if let Some(time_zone) = optional_string(args, &["time_zone", "timeZone"]) {
        if event.get("start").is_some() {
            event["start"]["timeZone"] = Value::String(time_zone.clone());
        }
        if event.get("end").is_some() {
            event["end"]["timeZone"] = Value::String(time_zone);
        }
    }
    for (arg, field) in [
        ("description", "description"),
        ("location", "location"),
        ("status", "status"),
        ("color_id", "colorId"),
        ("transparency", "transparency"),
        ("visibility", "visibility"),
    ] {
        if let Some(value) = optional_string(args, &[arg, field]) {
            event[field] = Value::String(value);
            inserted = true;
        }
    }
    if let Some(attendees) = optional_string_list(args, &["attendees"]) {
        event["attendees"] = Value::Array(
            attendees
                .into_iter()
                .map(|email| json!({ "email": email }))
                .collect(),
        );
        inserted = true;
    }
    if optional_bool(args, &["create_google_meet", "createGoogleMeet"]).unwrap_or(false) {
        event["conferenceData"] = json!({
            "createRequest": {
                "requestId": format!("aura-{}", uuid::Uuid::new_v4())
            }
        });
        inserted = true;
    }

    if !inserted {
        return Err(ToolError::ExternalToolError(format!(
            "{tool_name} requires at least one event field"
        )));
    }
    Ok(event)
}

fn google_calendar_event_result(response: &Value) -> Value {
    json!({
        "event": {
            "id": response.get("id").and_then(Value::as_str).unwrap_or_default(),
            "summary": response.get("summary").and_then(Value::as_str).unwrap_or_default(),
            "html_link": response.get("htmlLink").and_then(Value::as_str),
            "status": response.get("status").and_then(Value::as_str),
            "start": response.get("start").cloned().unwrap_or(Value::Null),
            "end": response.get("end").cloned().unwrap_or(Value::Null),
            "attendees": response.get("attendees").cloned().unwrap_or_else(|| json!([])),
            "conference_data": response.get("conferenceData").cloned().unwrap_or(Value::Null),
        }
    })
}

fn optional_bool(args: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_bool))
}

fn query_binding(arg_names: &[&str], target: &str) -> TrustedIntegrationArgBinding {
    TrustedIntegrationArgBinding {
        arg_names: arg_names.iter().map(|name| (*name).to_string()).collect(),
        target: target.to_string(),
        source: super::super::TrustedIntegrationArgSource::InputArgs,
        value_type: super::super::TrustedIntegrationArgValueType::String,
        required: false,
        default_value: None,
    }
}

fn default_query_binding(target: &str, default_value: Value) -> TrustedIntegrationArgBinding {
    TrustedIntegrationArgBinding {
        arg_names: Vec::new(),
        target: target.to_string(),
        source: super::super::TrustedIntegrationArgSource::InputArgs,
        value_type: super::super::TrustedIntegrationArgValueType::String,
        required: false,
        default_value: Some(default_value),
    }
}
