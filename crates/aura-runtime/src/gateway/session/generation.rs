//! Generation request handler — proxies image/3D generation through the
//! harness session to aura-router, translating router SSE events into typed
//! `OutboundMessage::Generation*` variants.

use super::Session;
use crate::protocol::{
    ErrorMsg, GenerationCompleted, GenerationErrorMsg, GenerationPartialImage,
    GenerationProgressMsg, GenerationRequest, GenerationStart, OutboundMessage,
};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Upstream connect timeout for the router generation endpoint.
///
/// Kept deliberately low — a slow router is worse than a fast failure so the
/// WS session can surface a `GenerationError` and the client can retry.
const GENERATION_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling for the initial HTTP request/response-header handshake.
const GENERATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time we will wait for another SSE byte before treating the stream
/// as stuck. Successful generations may run for minutes, but a healthy router
/// should emit progress, heartbeat, or completion data within this window.
const GENERATION_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) struct GenerationTurn {
    pub cancel_token: CancellationToken,
    pub join_handle: JoinHandle<()>,
}

struct RouterRequestSummary {
    body_bytes: usize,
    prompt_bytes: usize,
    model: Option<String>,
    size: Option<String>,
    image_count: usize,
    has_project_id: bool,
}

impl RouterRequestSummary {
    fn from_request(req: &GenerationRequest, body: &serde_json::Value) -> Self {
        Self {
            body_bytes: serde_json::to_vec(body).map_or(0, |bytes| bytes.len()),
            prompt_bytes: req.prompt.as_ref().map_or(0, String::len),
            model: req.model.clone(),
            size: req.size.clone(),
            image_count: req.images.as_ref().map_or(0, Vec::len),
            has_project_id: req.project_id.is_some(),
        }
    }
}

pub(super) fn start_generation(
    session: &Session,
    req: GenerationRequest,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
    router_url: &str,
) -> Option<GenerationTurn> {
    if !session.initialized {
        warn!(
            session_id = %session.session_id,
            mode = %req.mode,
            "Generation request rejected before session initialization"
        );
        let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
            code: "not_initialized".into(),
            message: "Start a run before sending generation_request".into(),
            recoverable: true,
            support_id: None,
        }));
        return None;
    }

    let mode = req.mode.clone();
    let auth_token = session.auth_token.clone().unwrap_or_default();

    let (url, body) = match build_router_request(router_url, &req) {
        Ok(pair) => pair,
        Err(msg) => {
            warn!(
                session_id = %session.session_id,
                mode = %mode,
                error = %msg,
                "Generation request rejected"
            );
            let _ = outbound_tx.try_send(OutboundMessage::Error(ErrorMsg {
                code: "invalid_mode".into(),
                message: msg,
                recoverable: true,
                support_id: None,
            }));
            return None;
        }
    };

    let cancel_token = CancellationToken::new();
    let cancel_for_task = cancel_token.clone();
    let outbound = outbound_tx.clone();
    let session_id = session.session_id.clone();
    let request_id = Uuid::new_v4().to_string();
    let summary = RouterRequestSummary::from_request(&req, &body);

    info!(
        %session_id,
        %request_id,
        %mode,
        url = %url,
        body_bytes = summary.body_bytes,
        prompt_bytes = summary.prompt_bytes,
        model = summary.model.as_deref().unwrap_or("missing"),
        size = summary.size.as_deref().unwrap_or("missing"),
        image_count = summary.image_count,
        has_project_id = summary.has_project_id,
        has_auth_token = !auth_token.is_empty(),
        "Generation turn started"
    );

    let join_handle = tokio::spawn(async move {
        run_generation_proxy(GenerationProxyCtx {
            session_id: &session_id,
            request_id: &request_id,
            url: &url,
            jwt: &auth_token,
            body: &body,
            mode: &mode,
            outbound: &outbound,
            cancel: cancel_for_task,
        })
        .await;
    });

    Some(GenerationTurn {
        cancel_token,
        join_handle,
    })
}

fn build_router_request(
    router_url: &str,
    req: &GenerationRequest,
) -> Result<(String, serde_json::Value), String> {
    match req.mode.as_str() {
        "image" => {
            let url = format!("{router_url}/v1/generate-image/stream");
            let mut body = serde_json::json!({});
            if let Some(ref prompt) = req.prompt {
                body["prompt"] = serde_json::json!(prompt);
            }
            if let Some(ref model) = req.model {
                body["model"] = serde_json::json!(model);
            }
            if let Some(ref size) = req.size {
                body["size"] = serde_json::json!(size);
            }
            if let Some(ref images) = req.images {
                body["images"] = serde_json::json!(images);
            }
            if let Some(ref pid) = req.project_id {
                body["projectId"] = serde_json::json!(pid);
            }
            if let Some(iter) = req.is_iteration {
                body["isIteration"] = serde_json::json!(iter);
            }
            Ok((url, body))
        }
        "3d" => {
            let url = format!("{router_url}/v1/generate-3d/stream");
            let mut body = serde_json::json!({});
            if let Some(ref image_url) = req.image_url {
                body["imageUrl"] = serde_json::json!(image_url);
            }
            if let Some(ref prompt) = req.prompt {
                body["prompt"] = serde_json::json!(prompt);
            }
            if let Some(ref pid) = req.project_id {
                body["projectId"] = serde_json::json!(pid);
            }
            Ok((url, body))
        }
        "video" => {
            let url = format!("{router_url}/v1/generate-video/stream");
            let mut body = serde_json::json!({});
            if let Some(ref prompt) = req.prompt {
                body["prompt"] = serde_json::json!(prompt);
            }
            if let Some(ref model) = req.model {
                body["model"] = serde_json::json!(model);
            }
            if let Some(ref aspect_ratio) = req.aspect_ratio {
                body["aspectRatio"] = serde_json::json!(aspect_ratio);
            }
            if let Some(duration) = req.duration_seconds {
                body["durationSeconds"] = serde_json::json!(duration);
            }
            if let Some(ref resolution) = req.resolution {
                body["resolution"] = serde_json::json!(resolution);
            }
            if let Some(audio) = req.generate_audio {
                body["generateAudio"] = serde_json::json!(audio);
            }
            if let Some(ref pid) = req.project_id {
                body["projectId"] = serde_json::json!(pid);
            }
            Ok((url, body))
        }
        other => Err(format!("Unknown generation mode: {other}")),
    }
}

/// Borrowed-everything context for a single generation proxy run.
///
/// Bundling the eight call-site parameters under one struct keeps
/// the function signature under the `clippy::too_many_arguments`
/// ceiling without cloning anything — every field is a `&'a`
/// borrow that lives at least as long as the `await`.
struct GenerationProxyCtx<'a> {
    session_id: &'a str,
    request_id: &'a str,
    url: &'a str,
    jwt: &'a str,
    body: &'a serde_json::Value,
    mode: &'a str,
    outbound: &'a mpsc::Sender<OutboundMessage>,
    cancel: CancellationToken,
}

async fn run_generation_proxy(ctx: GenerationProxyCtx<'_>) {
    let GenerationProxyCtx {
        session_id,
        request_id,
        url,
        jwt,
        body,
        mode,
        outbound,
        cancel,
    } = ctx;
    // Declared-Exception surface (see `docs/invariants.md`): this proxy is a
    // pure router-side SSE pipe, so we do not route through the kernel. We
    // still apply bounded connect / handshake timeouts so a hung upstream
    // cannot stall the WS session. The streaming body is guarded by an idle
    // timeout instead of a total timeout because real generations can
    // legitimately run for minutes as long as the router keeps sending events.
    let client = match reqwest::Client::builder()
        .connect_timeout(GENERATION_CONNECT_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!(
                %session_id,
                %request_id,
                %mode,
                error = %e,
                "Generation proxy: reqwest client build failed"
            );
            let _ = outbound.try_send(OutboundMessage::GenerationError(GenerationErrorMsg {
                code: "UPSTREAM_ERROR".into(),
                message: format!("failed to build http client: {e}"),
            }));
            return;
        }
    };
    info!(
        %session_id,
        %request_id,
        %mode,
        url = %url,
        body_bytes = serde_json::to_vec(body).map_or(0, |bytes| bytes.len()),
        handshake_timeout_secs = GENERATION_REQUEST_TIMEOUT.as_secs(),
        stream_idle_timeout_secs = GENERATION_STREAM_IDLE_TIMEOUT.as_secs(),
        "Generation proxy: sending upstream request"
    );
    let send_fut = client.post(url).bearer_auth(jwt).json(body).send();
    let resp = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            info!(
                %session_id,
                %request_id,
                %mode,
                "Generation cancelled by client before upstream handshake completed"
            );
            return;
        }
        result = tokio::time::timeout(GENERATION_REQUEST_TIMEOUT, send_fut) => result,
    };
    let resp = match resp {
        Ok(inner) => inner,
        Err(_) => {
            error!(
                %session_id,
                %request_id,
                %mode,
                timeout_secs = GENERATION_REQUEST_TIMEOUT.as_secs(),
                "Generation proxy: upstream request timed out during handshake"
            );
            let _ = outbound.try_send(OutboundMessage::GenerationError(GenerationErrorMsg {
                code: "UPSTREAM_TIMEOUT".into(),
                message: format!(
                    "upstream did not respond within {}s",
                    GENERATION_REQUEST_TIMEOUT.as_secs()
                ),
            }));
            return;
        }
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            error!(
                %session_id,
                %request_id,
                %mode,
                error = %e,
                "Generation proxy: upstream request failed"
            );
            let _ = outbound.try_send(OutboundMessage::GenerationError(GenerationErrorMsg {
                code: "UPSTREAM_ERROR".into(),
                message: format!("upstream request failed: {e}"),
            }));
            return;
        }
    };

    let status = resp.status();
    info!(
        %session_id,
        %request_id,
        %mode,
        %status,
        "Generation proxy: upstream response received"
    );

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Intentionally do NOT log `text`: upstream error bodies may include
        // provider secrets, unredacted prompts, or internal stack traces.
        // We surface `status` + a short code + length for diagnosis and send
        // the status-derived code to the client. (Wave 5 / T2.1.)
        error!(
            %session_id,
            %request_id,
            %mode,
            %status,
            body_len = text.len(),
            code = %format!("UPSTREAM_{}", status.as_u16()),
            "Generation proxy: upstream error"
        );
        let _ = outbound.try_send(OutboundMessage::GenerationError(GenerationErrorMsg {
            code: format!("UPSTREAM_{}", status.as_u16()),
            message: format!("upstream returned {status}"),
        }));
        return;
    }

    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut chunk_count: u64 = 0;
    let mut event_count: u64 = 0;
    let mut bytes_seen: u64 = 0;

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                info!(
                    %session_id,
                    %request_id,
                    %mode,
                    chunk_count,
                    event_count,
                    bytes_seen,
                    "Generation cancelled by client"
                );
                return;
            }
            chunk = tokio::time::timeout(GENERATION_STREAM_IDLE_TIMEOUT, byte_stream.next()) => {
                match chunk {
                    Err(_) => {
                        warn!(
                            %session_id,
                            %request_id,
                            %mode,
                            idle_timeout_secs = GENERATION_STREAM_IDLE_TIMEOUT.as_secs(),
                            chunk_count,
                            event_count,
                            bytes_seen,
                            "Generation proxy: upstream stream idle timeout"
                        );
                        let _ = outbound.try_send(OutboundMessage::GenerationError(
                            GenerationErrorMsg {
                                code: "STREAM_IDLE_TIMEOUT".into(),
                                message: format!(
                                    "upstream stream produced no data for {}s",
                                    GENERATION_STREAM_IDLE_TIMEOUT.as_secs()
                                ),
                            },
                        ));
                        return;
                    }
                    Ok(chunk) => match chunk {
                    Some(Ok(bytes)) => {
                        chunk_count += 1;
                        bytes_seen += bytes.len() as u64;
                        if chunk_count == 1 {
                            info!(
                                %session_id,
                                %request_id,
                                %mode,
                                first_chunk_bytes = bytes.len(),
                                "Generation proxy: first upstream stream chunk received"
                            );
                        } else {
                            debug!(
                                %session_id,
                                %request_id,
                                %mode,
                                chunk_count,
                                chunk_bytes = bytes.len(),
                                bytes_seen,
                                "Generation proxy: upstream stream chunk received"
                            );
                        }
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(frame) = pop_sse_frame(&mut buffer) {
                            if let Some((event_type, msg)) = parse_sse_frame(&frame, mode) {
                                event_count += 1;
                                log_generation_event(session_id, request_id, mode, &event_type, &msg, event_count);
                                if outbound.try_send(msg).is_err() {
                                    warn!(
                                        %session_id,
                                        %request_id,
                                        %mode,
                                        %event_type,
                                        "Generation proxy: outbound channel closed while forwarding event"
                                    );
                                    return;
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!(
                            %session_id,
                            %request_id,
                            %mode,
                            error = %e,
                            chunk_count,
                            event_count,
                            bytes_seen,
                            "Generation proxy: upstream stream error"
                        );
                        let _ = outbound.try_send(OutboundMessage::GenerationError(
                            GenerationErrorMsg {
                                code: "STREAM_ERROR".into(),
                                message: format!("Stream error: {e}"),
                            },
                        ));
                        return;
                    }
                    None => {
                        // Flush remaining buffer
                        if !buffer.trim().is_empty() {
                            if let Some((event_type, msg)) = parse_sse_frame(&buffer, mode) {
                                event_count += 1;
                                log_generation_event(session_id, request_id, mode, &event_type, &msg, event_count);
                                let _ = outbound.try_send(msg);
                            }
                        }
                        info!(
                            %session_id,
                            %request_id,
                            %mode,
                            chunk_count,
                            event_count,
                            bytes_seen,
                            "Generation proxy: upstream stream ended"
                        );
                        return;
                    }
                    }
                }
            }
        }
    }
}

fn pop_sse_frame(buffer: &mut String) -> Option<String> {
    let lf_pos = buffer.find("\n\n").map(|pos| (pos, 2));
    let crlf_pos = buffer.find("\r\n\r\n").map(|pos| (pos, 4));
    let (sep_pos, sep_len) = match (lf_pos, crlf_pos) {
        (Some(lf), Some(crlf)) => {
            if lf.0 < crlf.0 {
                lf
            } else {
                crlf
            }
        }
        (Some(lf), None) => lf,
        (None, Some(crlf)) => crlf,
        (None, None) => return None,
    };

    let frame = buffer[..sep_pos].to_string();
    *buffer = buffer[sep_pos + sep_len..].to_string();
    Some(frame)
}

fn parse_sse_frame(frame: &str, mode: &str) -> Option<(String, OutboundMessage)> {
    if frame.trim().is_empty() {
        return None;
    }
    let mut event_type = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    for raw_line in frame.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");

    if event_type.is_empty() {
        event_type = serde_json::from_str::<serde_json::Value>(&data)
            .ok()
            .and_then(|parsed| {
                parsed
                    .get("type")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "message".to_string());
    }

    translate_router_event(&event_type, &data, mode).map(|msg| (event_type, msg))
}

fn log_generation_event(
    session_id: &str,
    request_id: &str,
    mode: &str,
    event_type: &str,
    msg: &OutboundMessage,
    event_count: u64,
) {
    match msg {
        OutboundMessage::GenerationProgress(progress) => {
            info!(
                %session_id,
                %request_id,
                %mode,
                %event_type,
                event_count,
                percent = progress.percent,
                message_bytes = progress.message.len(),
                "Generation proxy: forwarding progress event"
            );
        }
        OutboundMessage::GenerationPartialImage(partial) => {
            info!(
                %session_id,
                %request_id,
                %mode,
                %event_type,
                event_count,
                data_bytes = partial.data.len(),
                "Generation proxy: forwarding partial image event"
            );
        }
        OutboundMessage::GenerationCompleted(_) => {
            info!(
                %session_id,
                %request_id,
                %mode,
                %event_type,
                event_count,
                "Generation proxy: forwarding completion event"
            );
        }
        OutboundMessage::GenerationError(error) => {
            warn!(
                %session_id,
                %request_id,
                %mode,
                %event_type,
                event_count,
                code = %error.code,
                message_bytes = error.message.len(),
                "Generation proxy: forwarding error event"
            );
        }
        _ => {
            debug!(
                %session_id,
                %request_id,
                %mode,
                %event_type,
                event_count,
                "Generation proxy: forwarding event"
            );
        }
    }
}

fn translate_router_event(event_type: &str, data: &str, mode: &str) -> Option<OutboundMessage> {
    match event_type {
        "start" => Some(OutboundMessage::GenerationStart(GenerationStart {
            mode: mode.to_string(),
        })),
        "progress" => {
            let parsed: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            Some(OutboundMessage::GenerationProgress(GenerationProgressMsg {
                percent: parsed
                    .get("percent")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                message: parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            }))
        }
        "partial-image" => {
            let parsed: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            Some(OutboundMessage::GenerationPartialImage(
                GenerationPartialImage {
                    data: parsed
                        .get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
            ))
        }
        "completed" => {
            let payload: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            Some(OutboundMessage::GenerationCompleted(GenerationCompleted {
                mode: mode.to_string(),
                payload,
            }))
        }
        "submitted" => {
            let parsed: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            let task_id = parsed.get("taskId").and_then(|v| v.as_str()).unwrap_or("");
            Some(OutboundMessage::GenerationProgress(GenerationProgressMsg {
                percent: 5.0,
                message: format!("Task submitted: {task_id}"),
            }))
        }
        "error" => {
            let parsed: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
            Some(OutboundMessage::GenerationError(GenerationErrorMsg {
                code: parsed
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("GENERATION_FAILED")
                    .to_string(),
                message: parsed
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Generation failed")
                    .to_string(),
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_sse_frame_handles_lf_and_crlf_boundaries() {
        let mut buffer =
            "event: start\ndata: {}\n\nevent: done\r\ndata: {}\r\n\r\ntail".to_string();

        assert_eq!(
            pop_sse_frame(&mut buffer).as_deref(),
            Some("event: start\ndata: {}")
        );
        assert_eq!(
            pop_sse_frame(&mut buffer).as_deref(),
            Some("event: done\r\ndata: {}")
        );
        assert_eq!(buffer, "tail");
        assert!(pop_sse_frame(&mut buffer).is_none());
    }

    #[test]
    fn parse_sse_frame_accepts_fields_without_space_after_colon() {
        let frame = r#"event:progress
data:{"percent":42,"message":"working"}"#;

        let (event_type, msg) = parse_sse_frame(frame, "image").expect("progress frame");

        assert_eq!(event_type, "progress");
        match msg {
            OutboundMessage::GenerationProgress(progress) => {
                assert_eq!(progress.percent, 42.0);
                assert_eq!(progress.message, "working");
            }
            other => panic!("expected progress message, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_frame_uses_json_type_when_event_line_is_missing() {
        let frame = r#"data: {"type":"completed","imageUrl":"https://example.test/image.png"}"#;

        let (event_type, msg) = parse_sse_frame(frame, "image").expect("completed frame");

        assert_eq!(event_type, "completed");
        match msg {
            OutboundMessage::GenerationCompleted(completed) => {
                assert_eq!(completed.mode, "image");
                assert_eq!(
                    completed
                        .payload
                        .get("imageUrl")
                        .and_then(|value| value.as_str()),
                    Some("https://example.test/image.png")
                );
            }
            other => panic!("expected completed message, got {other:?}"),
        }
    }
}
