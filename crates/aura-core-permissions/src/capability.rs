//! [`Capability`] — the enum of operations an agent can perform.

use serde::{Deserialize, Serialize};

/// Capabilities an agent can hold.
///
/// Serialized as an externally-tagged enum
/// (`{"type":"readProject","id":"..."}`) so the wire stays
/// forward-compatible when new variants land.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Capability {
    /// May call `spawn_agent` to create a subordinate agent.
    SpawnAgent,
    /// May call `send_to_agent` / `agent_lifecycle` / `delegate_task`
    /// on agents within scope.
    ControlAgent,
    /// May call `get_agent_state` on agents within scope.
    ReadAgent,
    /// May call `list_agents` to discover agents within scope.
    ListAgents,
    /// May add / remove org members.
    ManageOrgMembers,
    /// May mutate billing plans / invoices.
    ManageBilling,
    /// May invoke long-lived processes (shells, background jobs).
    InvokeProcess,
    /// May post into the activity feed.
    PostToFeed,
    /// May call media-generation tools (image / video / audio).
    GenerateMedia,
    /// May read project `id`.
    #[serde(rename_all = "camelCase")]
    ReadProject {
        /// Opaque project identifier.
        id: String,
    },
    /// May write project `id`.
    #[serde(rename_all = "camelCase")]
    WriteProject {
        /// Opaque project identifier.
        id: String,
    },
    /// Wildcard read across every project in the bundle's scope.
    ReadAllProjects,
    /// Wildcard write across every project in the bundle's scope.
    /// Strict superset of [`Capability::ReadAllProjects`].
    WriteAllProjects,
}

impl Capability {
    /// True iff `self` satisfies the project-scoped requirement
    /// `required`.
    ///
    /// Wildcard lifting rules:
    ///
    /// * `ReadProject { id }` is satisfied by any of:
    ///   - `ReadProject { id }` (exact),
    ///   - `WriteProject { id }` (write implies read),
    ///   - `ReadAllProjects` (wildcard),
    ///   - `WriteAllProjects` (wildcard write implies wildcard read).
    /// * `WriteProject { id }` is satisfied by:
    ///   - `WriteProject { id }` (exact),
    ///   - `WriteAllProjects` (wildcard).
    /// * For any other `required` the rule degenerates to exact
    ///   equality.
    #[must_use]
    pub fn satisfies(&self, required: &Capability) -> bool {
        match (self, required) {
            (held, req) if held == req => true,
            (Capability::ReadAllProjects, Capability::ReadProject { .. }) => true,
            (Capability::WriteAllProjects, Capability::ReadProject { .. }) => true,
            (Capability::WriteAllProjects, Capability::WriteProject { .. }) => true,
            (Capability::WriteProject { id: held_id }, Capability::ReadProject { id: req_id }) => {
                held_id == req_id
            }
            _ => false,
        }
    }

    /// Stable wire discriminant used by
    /// [`aura_core_modes::CapabilityProfile`] to express the per-mode
    /// ceiling without a circular `aura-*` dep.
    #[must_use]
    pub fn discriminant(&self) -> &'static str {
        match self {
            Capability::SpawnAgent => "spawnAgent",
            Capability::ControlAgent => "controlAgent",
            Capability::ReadAgent => "readAgent",
            Capability::ListAgents => "listAgents",
            Capability::ManageOrgMembers => "manageOrgMembers",
            Capability::ManageBilling => "manageBilling",
            Capability::InvokeProcess => "invokeProcess",
            Capability::PostToFeed => "postToFeed",
            Capability::GenerateMedia => "generateMedia",
            Capability::ReadProject { .. } => "readProject",
            Capability::WriteProject { .. } => "writeProject",
            Capability::ReadAllProjects => "readAllProjects",
            Capability::WriteAllProjects => "writeAllProjects",
        }
    }
}
