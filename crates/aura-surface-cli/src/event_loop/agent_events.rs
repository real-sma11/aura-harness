//! TUI forwarder: translates [`AgentLoopEvent`]s into [`UiCommand`]s.
//!
//! The per-variant routing lives in
//! [`aura_agent::map_agent_loop_event`]; this module only provides the
//! [`UiCommandSink`] implementation that owns the TUI-specific state
//! machine (streaming / thinking toggles, had-text tracking) and the
//! `mpsc::Sender<UiCommand>` that feeds the terminal.

use async_trait::async_trait;
use aura_agent::{map_agent_loop_event, AgentLoopEvent, TurnEventSink};
use aura_terminal::{events::ToolData, UiCommand};
use tokio::sync::mpsc;
use tracing::debug;

/// Tracks forwarder lifecycle so the caller can finalize streaming/thinking.
pub(crate) struct ForwarderState {
    pub streaming_active: bool,
    pub thinking_active: bool,
    pub had_text: bool,
}

/// Sink implementation that drains [`AgentLoopEvent`]s from the agent
/// loop and fans them out to the TUI as [`UiCommand`]s while maintaining
/// the streaming / thinking state machine the Ratatui layer depends on.
struct UiCommandSink {
    commands: mpsc::Sender<UiCommand>,
    state: ForwarderState,
}

impl UiCommandSink {
    async fn finish_thinking_if_active(&mut self) {
        if self.state.thinking_active {
            let _ = self.commands.send(UiCommand::FinishThinking).await;
            self.state.thinking_active = false;
        }
    }
}

#[async_trait]
impl TurnEventSink for UiCommandSink {
    async fn on_thinking_delta(&mut self, text: String) {
        if !self.state.thinking_active {
            let _ = self.commands.send(UiCommand::StartThinking).await;
            self.state.thinking_active = true;
        }
        let _ = self.commands.send(UiCommand::AppendThinking(text)).await;
    }

    async fn on_text_delta(&mut self, text: String) {
        self.finish_thinking_if_active().await;
        if !self.state.streaming_active {
            let _ = self.commands.send(UiCommand::StartStreaming).await;
            self.state.streaming_active = true;
        }
        self.state.had_text = true;
        let _ = self.commands.send(UiCommand::AppendText(text)).await;
    }

    async fn on_tool_start(&mut self, id: String, name: String) {
        self.finish_thinking_if_active().await;
        let _ = self
            .commands
            .send(UiCommand::ShowTool(ToolData {
                id,
                name,
                args: String::new(),
            }))
            .await;
    }

    async fn on_tool_input_snapshot(&mut self, id: String, _name: String, _input: String) {
        debug!(tool_id = %id, "Tool input streaming");
    }

    async fn on_tool_result(
        &mut self,
        tool_use_id: String,
        _tool_name: String,
        content: String,
        is_error: bool,
    ) {
        let _ = self
            .commands
            .send(UiCommand::CompleteTool {
                id: tool_use_id,
                result: content,
                success: !is_error,
            })
            .await;
    }

    async fn on_stream_reset(&mut self, reason: String) {
        debug!(reason = %reason, "Stream reset received");
        if self.state.streaming_active {
            let _ = self.commands.send(UiCommand::FinishStreaming).await;
            self.state.streaming_active = false;
        }
        if self.state.thinking_active {
            let _ = self.commands.send(UiCommand::FinishThinking).await;
            self.state.thinking_active = false;
        }
        self.state.had_text = false;
    }

    async fn on_warning(&mut self, message: String) {
        let _ = self.commands.send(UiCommand::ShowWarning(message)).await;
    }

    async fn on_error(&mut self, _code: String, message: String, _recoverable: bool) {
        let _ = self.commands.send(UiCommand::ShowWarning(message)).await;
    }

    // `ThinkingComplete`, `IterationComplete`, `StepComplete`,
    // `ToolComplete`, `Debug` — intentionally use the default no-op
    // impls; the TUI derives UI transitions from the deltas above.
}

/// Reads [`AgentLoopEvent`]s and translates them into [`UiCommand`]s.
pub(crate) async fn forward_agent_events(
    mut rx: tokio::sync::mpsc::Receiver<AgentLoopEvent>,
    commands: mpsc::Sender<UiCommand>,
) -> ForwarderState {
    let mut sink = UiCommandSink {
        commands,
        state: ForwarderState {
            streaming_active: false,
            thinking_active: false,
            had_text: false,
        },
    };

    while let Some(event) = rx.recv().await {
        map_agent_loop_event(event, &mut sink).await;
    }

    sink.state
}
