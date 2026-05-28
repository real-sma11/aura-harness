//! Distinguishable verdict + legacy-compat shim returned by the policy
//! authorization pipeline.

use crate::PendingToolPrompt;

/// Distinguishable verdict returned by the tool authorization pipeline.
///
/// Phase 6 (security audit) split what used to be "allowed / not allowed"
/// into three cases so downstream code can differentiate a permanent
/// deny from a proposal that is waiting on an out-of-band operator
/// approval. `Allow` carries no reason because "allowed" is the sole
/// happy path; the other two always carry a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Allowed to proceed.
    Allow,
    /// Denied at the policy layer pending a live approval prompt.
    RequireApproval {
        /// Human-readable reason, e.g. `"Tool 'run_command' requires approval"`.
        reason: String,
        /// Structured prompt metadata for live tri-state `ask` prompts.
        prompt: Option<PendingToolPrompt>,
    },
    /// Permanently denied. No approval will unlock it.
    Deny {
        /// Human-readable reason, e.g. `"Tool 'foo' is not allowed"`.
        reason: String,
    },
}

impl PolicyVerdict {
    /// `true` iff the verdict is [`PolicyVerdict::Allow`].
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Extract the reason string, if any. `Allow` has none.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::RequireApproval { reason, .. } | Self::Deny { reason } => Some(reason.as_str()),
        }
    }
}

/// Result of policy check.
///
/// Legacy compat shim around [`PolicyVerdict`]: `allowed` is exactly
/// `verdict.is_allowed()`. Downstream code that needs to branch on
/// "approval required" vs "hard deny" should switch to [`PolicyVerdict`]
/// directly via the `*_verdict` variants of the `Policy` methods.
#[derive(Debug, Clone)]
pub struct PolicyResult {
    /// Whether the proposal is allowed.
    pub allowed: bool,
    /// Reason for rejection (if not allowed).
    pub reason: Option<String>,
    /// Structured verdict this `PolicyResult` was derived from. Phase 6
    /// additions (e.g. `process_tool_proposal`) should match on this
    /// instead of `allowed`.
    pub verdict: PolicyVerdict,
}

impl From<PolicyVerdict> for PolicyResult {
    fn from(verdict: PolicyVerdict) -> Self {
        let allowed = verdict.is_allowed();
        let reason = verdict.reason().map(std::string::ToString::to_string);
        Self {
            allowed,
            reason,
            verdict,
        }
    }
}
