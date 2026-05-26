//! Custom tracing event formatter for the structured console transcript.
//!
//! Recognises events that opt into the visual-block layout via the
//! dedicated `"aura::console"` target (set by
//! [`aura_agent::console`] and [`aura_reasoner::console`]) and writes
//! their message verbatim — no level / target / span list / field
//! key=value pairs — so the box-drawing blocks render cleanly.
//!
//! Every other event falls back to a compact one-line format that
//! keeps the span chain (`agent{id}:worker:task{id}:turn{T}:sampling{I}`)
//! visible as a prefix.

use std::fmt::Write;

use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

/// Targets the formatter recognises as "structured console" events.
/// Must stay in sync with [`aura_agent::console::CONSOLE_TARGET`] and
/// [`aura_reasoner::console::CONSOLE_TARGET`].
const CONSOLE_TARGETS: &[&str] = &["aura::console"];

/// Tracing event formatter that renders the structured-console blocks
/// verbatim and everything else as a compact one-liner with span chain.
#[derive(Default)]
pub struct AuraConsoleFormat;

impl AuraConsoleFormat {
    /// Construct the formatter with a short `HH:MM:SS.mmm` timestamp.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Render `HH:MM:SS.mmm` derived from `std::time::SystemTime::now()`.
/// Hand-rolled (rather than reaching for `chrono`) so the formatter
/// stays dependency-free; the rest of the workspace already pulls in
/// `chrono` for typed timestamps, but enabling the
/// `tracing-subscriber/chrono` feature would force every
/// `tracing-subscriber` consumer in the tree to rebuild.
struct HmsTime;

impl FormatTime for HmsTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        write!(w, "{h:02}:{m:02}:{s:02}.{millis:03}")
    }
}

impl<S, N> FormatEvent<S, N> for AuraConsoleFormat
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let metadata = event.metadata();

        let timer = HmsTime;
        if CONSOLE_TARGETS.contains(&metadata.target()) {
            // Visual-block events: render the message verbatim. The
            // block already encodes its own context; we only prepend a
            // short timestamp so an operator scanning a file can tell
            // when each block landed.
            let mut ts_buf = String::new();
            let mut ts_writer = Writer::new(&mut ts_buf);
            timer.format_time(&mut ts_writer).ok();
            let timestamp = ts_buf.trim_end();
            if !timestamp.is_empty() {
                write!(writer, "{timestamp}  ")?;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            writeln!(writer, "{}", visitor.message)?;
            return Ok(());
        }

        // Compact fallback for ordinary events.
        let mut ts_buf = String::new();
        let mut ts_writer = Writer::new(&mut ts_buf);
        timer.format_time(&mut ts_writer).ok();
        write!(writer, "{}  ", ts_buf.trim_end())?;

        let level = metadata.level();
        write!(writer, "{level:>5}  ")?;

        // Span chain: `agent{id=…}:worker:task{id=…}:turn{0}:sampling{4}`.
        // Walk leaf → root via `ctx.event_scope()` and stitch them with
        // colons so the order matches reading direction.
        if let Some(scope) = ctx.event_scope() {
            let mut first = true;
            let mut chain = String::new();
            for span in scope.from_root() {
                if !first {
                    chain.push(':');
                }
                first = false;
                chain.push_str(span.name());
                let ext = span.extensions();
                if let Some(fields) = ext.get::<FormattedFields<N>>() {
                    if !fields.fields.is_empty() {
                        let _ = write!(chain, "{{{}}}", fields.fields);
                    }
                }
            }
            if !chain.is_empty() {
                write!(writer, "{chain}  ")?;
            }
        }

        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `{:?}` on `&str` would produce `"…"` (quoted); use the
            // alternate form so multi-line messages keep their layout.
            let formatted = format!("{value:?}");
            // strip leading/trailing quote if the debug wrapped a string
            let trimmed = formatted
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .map(|s| s.replace("\\n", "\n").replace("\\\"", "\""))
                .unwrap_or(formatted);
            self.message = trimmed;
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;
    use tracing_subscriber::fmt::layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    fn collect(write_fn: impl FnOnce()) -> String {
        // Capture output via a `MakeWriter` that pushes into a shared
        // `Vec<u8>` so the assertion can inspect the rendered bytes.
        use std::sync::{Arc, Mutex};
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_for_writer = Arc::clone(&buf);
        let make_writer = move || VecWriter {
            inner: Arc::clone(&buf_for_writer),
        };
        let subscriber = tracing_subscriber::registry().with(
            layer()
                .event_format(AuraConsoleFormat::new())
                .with_writer(make_writer),
        );
        let _g = subscriber.set_default();
        write_fn();
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    struct VecWriter {
        inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.inner.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn console_target_renders_message_verbatim() {
        let out = collect(|| {
            tracing::info!(
                target: "aura::console",
                "┌─ → POST /v1/messages\n│   model  claude-opus-4-6\n└─"
            );
        });
        assert!(
            out.contains("┌─ → POST /v1/messages"),
            "expected box header, got: {out}"
        );
        assert!(out.contains("└─"), "expected box footer, got: {out}");
        // No level / target prefix should appear:
        assert!(!out.contains("INFO"), "unexpected level prefix: {out}");
        assert!(
            !out.contains("aura::console"),
            "unexpected target leak: {out}"
        );
    }

    #[test]
    fn ordinary_event_keeps_compact_format() {
        let out = collect(|| {
            tracing::event!(Level::INFO, foo = "bar", "ordinary message");
        });
        assert!(
            out.contains("ordinary message"),
            "expected message, got: {out}"
        );
        assert!(
            out.contains("foo=\"bar\"") || out.contains("foo=bar"),
            "expected field, got: {out}"
        );
        assert!(out.contains("INFO"), "expected level prefix, got: {out}");
    }
}
