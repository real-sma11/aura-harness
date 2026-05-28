//! Pure resolution math: [`narrow`], [`intersect`], [`allows`],
//! [`allows_tool`].
//!
//! # Invariants
//!
//! - `narrow(p, c) ⊆ p ∩ c` for any `p, c`.
//! - `intersect(a, b) == intersect(b, a)` (commutative).
//! - `intersect(a, intersect(b, c)) == intersect(intersect(a, b), c)`
//!   (associative).
//! - `allows(narrow(p, c), x) → allows(p, x) ∧ allows(c, x)`.

use crate::capability::Capability;
use crate::Permissions;

/// Intersect parent and child permission bundles ("narrowing").
///
/// The result is a strict subset of both inputs — the canonical
/// derivation primitive for spawning a subagent: the child receives
/// only what the parent grants AND the child explicitly retains.
#[must_use]
pub fn narrow(parent: &Permissions, child: &Permissions) -> Permissions {
    intersect(parent, child)
}

/// Set intersection of capability bundles.
///
/// Commutative and associative.
#[must_use]
pub fn intersect(a: &Permissions, b: &Permissions) -> Permissions {
    let scope = a.scope.intersect(&b.scope);
    let mut capabilities: Vec<Capability> = Vec::new();
    for cap in &a.capabilities {
        if b.capabilities.iter().any(|other| other == cap) && !capabilities.contains(cap) {
            capabilities.push(cap.clone());
        }
    }
    Permissions {
        scope,
        capabilities,
    }
}

/// True iff `perms` holds a grant that satisfies `cap` (consulting
/// the wildcard lifting rules in [`Capability::satisfies`]).
#[must_use]
pub fn allows(perms: &Permissions, cap: &Capability) -> bool {
    perms.capabilities.iter().any(|held| held.satisfies(cap))
}

/// True iff `perms` allows the named tool.
///
/// Phase 1 stub: returns `true` if any of the well-known
/// tool-name-bearing capabilities is granted; per-tool enforcement is
/// the job of the higher-layer policy gate. This shape keeps the
/// public API stable while the executor layer matures.
#[must_use]
pub fn allows_tool(perms: &Permissions, tool: &str) -> bool {
    match tool {
        "spawn_agent" => allows(perms, &Capability::SpawnAgent),
        "list_agents" => allows(perms, &Capability::ListAgents),
        "get_agent_state" => allows(perms, &Capability::ReadAgent),
        "send_to_agent" | "agent_lifecycle" | "delegate_task" => {
            allows(perms, &Capability::ControlAgent)
        }
        "run_command" | "shell" => allows(perms, &Capability::InvokeProcess),
        _ => {
            // Unrecognised tools default to permitted — the executor
            // policy gate decides; this helper is intentionally a
            // lower bound.
            true
        }
    }
}
