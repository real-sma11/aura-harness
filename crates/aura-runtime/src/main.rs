//! Aura Node binary entry point.

use aura_runtime::console_format::AuraConsoleFormat;
use aura_runtime::{Node, NodeConfig};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(fmt::layer().event_format(AuraConsoleFormat::new()))
        .with(EnvFilter::from_default_env().add_directive("aura=info".parse()?))
        .init();

    // Phase D (harness v2.2): top-level signal handler so external
    // Ctrl+C produces a deterministic log line + exit code 130
    // instead of the mysterious `0xFFFFFFFF` (= -1 unsigned) that
    // previously appeared with zero panic/abort/unwind diagnostics —
    // indistinguishable from a real crash. `Node::run` already
    // installs its own `with_graceful_shutdown(shutdown_signal())`
    // on the axum server (see `node::shutdown_signal`), which drains
    // in-flight HTTP requests on the first Ctrl+C; this handler is
    // the belt-and-suspenders hard deadline: if axum has not drained
    // within 2s, we exit(130) anyway so the process never silently
    // hangs and the operator always sees an exit cause in the log.
    //
    // `tokio::signal::ctrl_c` on Windows wraps `SetConsoleCtrlHandler`
    // for CTRL_C_EVENT; no platform `#[cfg]` is needed for the basic
    // case. The Windows-only Ctrl+Break branch below covers
    // CTRL_BREAK_EVENT for parity with the axum shutdown signal.
    //
    // Future improvement: thread a single top-level
    // `CancellationToken` through `Node::run` -> `RouterState` ->
    // per-session generation tokens so this handler can `.cancel()`
    // active LLM requests before the 2s timeout, instead of the per-
    // session tokens that live inside `session::ws_handler` /
    // `session::generation` today. Not in scope for Phase D — the
    // hard exit alone fixes the diagnosability problem.
    tokio::spawn(async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                tracing::warn!("received Ctrl+C; initiating graceful shutdown");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                tracing::warn!("graceful shutdown timeout reached; exiting with code 130");
                std::process::exit(130);
            }
            Err(err) => {
                tracing::error!(?err, "failed to install Ctrl+C handler");
            }
        }
    });

    #[cfg(windows)]
    {
        match tokio::signal::windows::ctrl_break() {
            Ok(mut stream) => {
                tokio::spawn(async move {
                    if stream.recv().await.is_some() {
                        tracing::warn!("received Ctrl+Break; initiating graceful shutdown");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        tracing::warn!("graceful shutdown timeout reached; exiting with code 130");
                        std::process::exit(130);
                    }
                });
            }
            Err(err) => {
                tracing::error!(?err, "failed to install Ctrl+Break handler");
            }
        }
    }

    let config = NodeConfig::from_env();

    // Run the node
    let result = Node::new(config).run().await;

    // Phase D: always emit a clean-exit line so log tails show a
    // cause for the process going away — either the Ctrl+C warning
    // above or this info line. Never silence.
    tracing::info!("aura-node exiting cleanly");

    result
}
