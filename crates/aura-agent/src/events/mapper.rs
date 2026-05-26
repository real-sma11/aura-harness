//! Dispatch [`AgentLoopEvent`]s onto a caller-supplied [`TurnEventSink`].
//!
//! Two consumers drive the agent loop today: the TUI's forwarder in
//! `src/event_loop/agent_events.rs` and the WebSocket session's
//! `forward_events_to_ws` in `crates/aura-runtime/src/session/helpers.rs`.
//! Both used to contain hand-written matches over every [`AgentLoopEvent`]
//! variant — which is fine until the enum grows a new variant and one
//! side silently drops it.
//!
//! [`map_agent_loop_event`] centralises the fan-out. Each event variant
//! maps 1:1 to a method on [`TurnEventSink`]; adding a new variant is a
//! compile error until every sink adds (or explicitly defaults) the
//! corresponding hook. Sinks stay in charge of their own side-effects
//! (mpsc sends, partial-JSON parsing, streaming state machines) — this
//! module is only responsible for "which hook is called for which
//! variant" and the exhaustive-match enforcement that goes with it.
//!
//! # Why async?
//!
//! The TUI forwarder awaits backpressure on its `mpsc::Sender<UiCommand>`
//! so a slow UI throttles the agent loop. Modeling hooks as `async`
//! methods lets that sink `.await` while the headless WS sink
//! (`try_send`-based) keeps its methods effectively synchronous. We use
//! `async_trait` because this is an object-safety-adjacent surface — in
//! practice callers pass `&mut impl TurnEventSink` by generic, but the
//! macro keeps the ergonomics identical either way.

use crate::events::{AgentLoopEvent, DebugEvent};
use async_trait::async_trait;

/// Callback hooks a concrete consumer implements to render or forward
/// [`AgentLoopEvent`]s.
///
/// Every method has a no-op default so a consumer can ignore variants
/// it doesn't care about (e.g. the TUI currently logs `ToolInputSnapshot`
/// without emitting a UI command). The exhaustive match in
/// [`map_agent_loop_event`] guarantees a compile error when the event
/// enum grows — the default impls only skip hooks, they do not skip
/// variants.
#[async_trait]
pub trait TurnEventSink: Send {
    async fn on_text_delta(&mut self, _text: String) {}
    async fn on_thinking_delta(&mut self, _text: String) {}
    async fn on_thinking_complete(&mut self) {}
    async fn on_tool_start(&mut self, _id: String, _name: String) {}
    async fn on_tool_input_snapshot(&mut self, _id: String, _name: String, _input: String) {}
    async fn on_tool_result(
        &mut self,
        _tool_use_id: String,
        _tool_name: String,
        _content: String,
        _is_error: bool,
    ) {
    }
    async fn on_tool_complete(
        &mut self,
        _name: String,
        _args: Option<serde_json::Value>,
        _result: String,
        _is_error: bool,
    ) {
    }
    async fn on_tool_call_completed(
        &mut self,
        _tool_use_id: String,
        _tool_name: String,
        _input: serde_json::Value,
        _is_error: bool,
    ) {
    }
    async fn on_iteration_complete(
        &mut self,
        _iteration: usize,
        _input_tokens: u64,
        _output_tokens: u64,
    ) {
    }
    async fn on_step_complete(&mut self) {}
    async fn on_stream_reset(&mut self, _reason: String) {}
    async fn on_warning(&mut self, _message: String) {}
    async fn on_error(&mut self, _code: String, _message: String, _recoverable: bool) {}
    /// Heartbeat / status hook. Default no-op so sinks that don't
    /// care about progress frames (TUI in non-interactive mode, in-process
    /// tests) keep compiling without changes; the WS sink overrides this
    /// to forward the frame onto the harness wire.
    async fn on_progress(
        &mut self,
        _stage: String,
        _tool_name: Option<String>,
        _elapsed_ms: Option<u64>,
        _message: Option<String>,
    ) {
    }
    async fn on_tool_call_retrying(
        &mut self,
        _tool_use_id: String,
        _tool_name: String,
        _attempt: u32,
        _max_attempts: u32,
        _delay_ms: u64,
        _reason: String,
    ) {
    }
    async fn on_tool_call_failed(
        &mut self,
        _tool_use_id: String,
        _tool_name: String,
        _reason: String,
    ) {
    }
    /// Best-effort test-suite outcome after `task_done` (Codex parity).
    /// Default no-op so sinks that don't surface DoD telemetry keep
    /// compiling without changes.
    async fn on_test_suite_warning(
        &mut self,
        _passed: bool,
        _summary: String,
        _failed_tests: Vec<String>,
    ) {
    }
    async fn on_debug(&mut self, _event: DebugEvent) {}
}

/// Dispatch a single [`AgentLoopEvent`] onto `sink`.
///
/// Exhaustive over every `AgentLoopEvent` variant — if the enum grows
/// a new variant this function stops compiling, forcing the author to
/// decide whether the new variant needs a sink hook.
pub async fn map_agent_loop_event<S>(event: AgentLoopEvent, sink: &mut S)
where
    S: TurnEventSink + ?Sized,
{
    match event {
        AgentLoopEvent::TextDelta(text) => sink.on_text_delta(text).await,
        AgentLoopEvent::ThinkingDelta(text) => sink.on_thinking_delta(text).await,
        AgentLoopEvent::ThinkingComplete => sink.on_thinking_complete().await,
        AgentLoopEvent::ToolStart { id, name } => sink.on_tool_start(id, name).await,
        AgentLoopEvent::ToolInputSnapshot { id, name, input } => {
            sink.on_tool_input_snapshot(id, name, input).await;
        }
        AgentLoopEvent::ToolResult {
            tool_use_id,
            tool_name,
            content,
            is_error,
        } => {
            sink.on_tool_result(tool_use_id, tool_name, content, is_error)
                .await;
        }
        AgentLoopEvent::ToolComplete {
            name,
            args,
            result,
            is_error,
        } => sink.on_tool_complete(name, args, result, is_error).await,
        AgentLoopEvent::ToolCallCompleted {
            tool_use_id,
            tool_name,
            input,
            is_error,
        } => {
            sink.on_tool_call_completed(tool_use_id, tool_name, input, is_error)
                .await;
        }
        AgentLoopEvent::IterationComplete {
            iteration,
            input_tokens,
            output_tokens,
        } => {
            sink.on_iteration_complete(iteration, input_tokens, output_tokens)
                .await;
        }
        AgentLoopEvent::StepComplete => sink.on_step_complete().await,
        AgentLoopEvent::StreamReset { reason } => sink.on_stream_reset(reason).await,
        AgentLoopEvent::Warning(message) => sink.on_warning(message).await,
        AgentLoopEvent::Error {
            code,
            message,
            recoverable,
        } => sink.on_error(code, message, recoverable).await,
        AgentLoopEvent::Progress {
            stage,
            tool_name,
            elapsed_ms,
            message,
        } => {
            sink.on_progress(stage, tool_name, elapsed_ms, message)
                .await;
        }
        AgentLoopEvent::ToolCallRetrying {
            tool_use_id,
            tool_name,
            attempt,
            max_attempts,
            delay_ms,
            reason,
        } => {
            sink.on_tool_call_retrying(
                tool_use_id,
                tool_name,
                attempt,
                max_attempts,
                delay_ms,
                reason,
            )
            .await;
        }
        AgentLoopEvent::ToolCallFailed {
            tool_use_id,
            tool_name,
            reason,
        } => {
            sink.on_tool_call_failed(tool_use_id, tool_name, reason)
                .await;
        }
        AgentLoopEvent::TestSuiteWarning {
            passed,
            summary,
            failed_tests,
        } => {
            sink.on_test_suite_warning(passed, summary, failed_tests)
                .await;
        }
        AgentLoopEvent::Debug(event) => sink.on_debug(event).await,
    }
}

#[cfg(test)]
mod mapper_tests {
    use super::*;

    /// Fake sink that records each hook invocation as a string tag so
    /// the test can assert both variant coverage and arrival order.
    #[derive(Default)]
    struct RecordingSink {
        calls: Vec<String>,
    }

    #[async_trait]
    impl TurnEventSink for RecordingSink {
        async fn on_text_delta(&mut self, text: String) {
            self.calls.push(format!("text:{text}"));
        }
        async fn on_thinking_delta(&mut self, text: String) {
            self.calls.push(format!("think:{text}"));
        }
        async fn on_thinking_complete(&mut self) {
            self.calls.push("think_done".into());
        }
        async fn on_tool_start(&mut self, id: String, name: String) {
            self.calls.push(format!("tool_start:{id}:{name}"));
        }
        async fn on_tool_input_snapshot(&mut self, id: String, name: String, input: String) {
            self.calls.push(format!("tool_snap:{id}:{name}:{input}"));
        }
        async fn on_tool_result(
            &mut self,
            tool_use_id: String,
            tool_name: String,
            _content: String,
            is_error: bool,
        ) {
            self.calls
                .push(format!("tool_res:{tool_use_id}:{tool_name}:{is_error}"));
        }
        async fn on_tool_complete(
            &mut self,
            name: String,
            _args: Option<serde_json::Value>,
            _result: String,
            is_error: bool,
        ) {
            self.calls.push(format!("tool_done:{name}:{is_error}"));
        }
        async fn on_iteration_complete(
            &mut self,
            iteration: usize,
            _input_tokens: u64,
            _output_tokens: u64,
        ) {
            self.calls.push(format!("iter:{iteration}"));
        }
        async fn on_step_complete(&mut self) {
            self.calls.push("step_done".into());
        }
        async fn on_stream_reset(&mut self, reason: String) {
            self.calls.push(format!("reset:{reason}"));
        }
        async fn on_warning(&mut self, message: String) {
            self.calls.push(format!("warn:{message}"));
        }
        async fn on_error(&mut self, code: String, _message: String, recoverable: bool) {
            self.calls.push(format!("err:{code}:{recoverable}"));
        }
        async fn on_debug(&mut self, event: DebugEvent) {
            self.calls.push(format!("debug:{}", event.kind()));
        }
    }

    #[tokio::test]
    async fn maps_text_and_tool_sequence_in_order() {
        let mut sink = RecordingSink::default();
        let events = vec![
            AgentLoopEvent::ThinkingDelta("planning".into()),
            AgentLoopEvent::ThinkingComplete,
            AgentLoopEvent::ToolStart {
                id: "t1".into(),
                name: "read_file".into(),
            },
            AgentLoopEvent::ToolInputSnapshot {
                id: "t1".into(),
                name: "read_file".into(),
                input: "{\"path\":\"a\"}".into(),
            },
            AgentLoopEvent::ToolResult {
                tool_use_id: "t1".into(),
                tool_name: "read_file".into(),
                content: "ok".into(),
                is_error: false,
            },
            AgentLoopEvent::TextDelta("hello".into()),
            AgentLoopEvent::IterationComplete {
                iteration: 0,
                input_tokens: 10,
                output_tokens: 20,
            },
            AgentLoopEvent::StepComplete,
        ];

        for ev in events {
            map_agent_loop_event(ev, &mut sink).await;
        }

        assert_eq!(
            sink.calls,
            vec![
                "think:planning",
                "think_done",
                "tool_start:t1:read_file",
                "tool_snap:t1:read_file:{\"path\":\"a\"}",
                "tool_res:t1:read_file:false",
                "text:hello",
                "iter:0",
                "step_done",
            ]
        );
    }

    #[tokio::test]
    async fn stream_reset_and_error_forward_reason_and_code() {
        let mut sink = RecordingSink::default();
        map_agent_loop_event(
            AgentLoopEvent::StreamReset {
                reason: "provider_drop".into(),
            },
            &mut sink,
        )
        .await;
        map_agent_loop_event(
            AgentLoopEvent::Error {
                code: "E_RATE".into(),
                message: "rate-limited".into(),
                recoverable: true,
            },
            &mut sink,
        )
        .await;
        map_agent_loop_event(AgentLoopEvent::Warning("watch out".into()), &mut sink).await;

        assert_eq!(
            sink.calls,
            vec!["reset:provider_drop", "err:E_RATE:true", "warn:watch out"]
        );
    }

    #[tokio::test]
    async fn unhandled_hooks_default_to_noop() {
        // A sink that overrides nothing silently drops events — the
        // default impls must not panic or crash the dispatcher.
        struct Silent;
        #[async_trait]
        impl TurnEventSink for Silent {}

        let mut sink = Silent;
        map_agent_loop_event(AgentLoopEvent::StepComplete, &mut sink).await;
        map_agent_loop_event(AgentLoopEvent::ThinkingComplete, &mut sink).await;
        // If we got here without panicking, the defaults are wired.
    }
}
