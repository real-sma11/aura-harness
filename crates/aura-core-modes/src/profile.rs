//! Bundled session [`ModeProfile`].

use serde::{Deserialize, Serialize};

use crate::modes::{AgentMode, KernelMode, ReplayMode, SandboxMode};

/// The bundled per-session mode configuration.
///
/// One instance per agent. Resolved once at session start by the
/// surface binary and propagated unchanged to every derived child.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeProfile {
    /// Headline agent mode (`Agent`/`Plan`/`Ask`/`Debug`).
    pub agent: AgentMode,
    /// Kernel audit-payload tier.
    pub kernel: KernelMode,
    /// Exec sandbox profile.
    pub sandbox: SandboxMode,
    /// Replay state (`Live` for normal sessions).
    pub replay: ReplayMode,
}
