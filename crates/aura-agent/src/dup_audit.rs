//! Duplicate-`tool_result` audit instrumentation.
//!
//! Scans `state.messages` for `ContentBlock::ToolResult` blocks that
//! share a `tool_use_id` and emits a `tracing::error!` per duplicate
//! id, capturing a backtrace so we can identify the call site that
//! introduced the duplicate. Used to diagnose the recurring
//! `convert_messages_to_api: deduplicated tool_result blocks` warnings
//! whose root cause is a duplicate that persists in the agent's
//! internal message vector across iterations.
//!
//! `Backtrace::capture` is a no-op unless `RUST_BACKTRACE=1` (or
//! `RUST_LIB_BACKTRACE=1`) is set in the environment, so the runtime
//! cost on a clean conversation is just a `HashMap` build per audit
//! call.

use aura_reasoner::{ContentBlock, Message};
use std::backtrace::Backtrace;
use std::collections::HashMap;
use tracing::error;

/// Scan `messages` and emit a `tracing::error!` for every
/// `tool_use_id` that appears more than once across all
/// `ContentBlock::ToolResult` blocks.
///
/// `source` identifies the call site so the first audit that fires
/// after a duplicate appears names the introducing path. Pair `pre`
/// and `post` audits around mutations to bisect.
pub(crate) fn audit_tool_result_duplicates(messages: &[Message], source: &'static str) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                *counts.entry(tool_use_id.as_str()).or_default() += 1;
            }
        }
    }
    for (id, n) in counts.into_iter().filter(|(_, n)| *n > 1) {
        let bt = Backtrace::capture();
        error!(
            target: "aura::dup_audit",
            source,
            tool_use_id = id,
            count = n,
            backtrace = %bt,
            "duplicate ToolResult detected in state.messages"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_reasoner::{Role, ToolResultContent};
    use tracing::subscriber::with_default;
    use tracing_subscriber::fmt::MakeWriter;
    use std::io;
    use std::sync::{Arc, Mutex};

    /// In-memory `MakeWriter` so tests can assert on the exact
    /// `tracing` event payload without a global subscriber.
    #[derive(Clone, Default)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::tool_result(id, ToolResultContent::text("ok"), false)
    }

    #[test]
    fn emits_event_for_duplicate_tool_use_id() {
        let writer = CapturedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .finish();

        let messages = vec![
            Message::new(Role::User, vec![tool_result("toolu_dup")]),
            Message::new(Role::Assistant, vec![ContentBlock::text("ok")]),
            Message::new(Role::User, vec![tool_result("toolu_dup")]),
        ];

        with_default(subscriber, || {
            audit_tool_result_duplicates(&messages, "test_source");
        });

        let captured = writer.0.lock().unwrap();
        let s = String::from_utf8_lossy(&captured);
        assert!(
            s.contains("duplicate ToolResult detected"),
            "expected duplicate event, got: {s}"
        );
        assert!(s.contains("source=\"test_source\""), "missing source label: {s}");
        assert!(s.contains("tool_use_id=\"toolu_dup\""), "missing id field: {s}");
        assert!(s.contains("count=2"), "missing count field: {s}");
    }

    #[test]
    fn no_event_when_all_ids_unique() {
        let writer = CapturedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer.clone())
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .finish();

        let messages = vec![
            Message::new(Role::User, vec![tool_result("a"), tool_result("b")]),
            Message::new(Role::User, vec![tool_result("c")]),
        ];

        with_default(subscriber, || {
            audit_tool_result_duplicates(&messages, "unique");
        });

        let captured = writer.0.lock().unwrap();
        assert!(
            captured.is_empty(),
            "no event should fire for all-unique ids: {}",
            String::from_utf8_lossy(&captured)
        );
    }
}
