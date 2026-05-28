//! [`AgentScope`] — the orgs/projects/agent ids an agent may touch.

use serde::{Deserialize, Serialize};

/// The universe of orgs / projects / agents an agent may touch.
///
/// An empty list on every axis means **universe** (no scope
/// restriction). Non-empty lists explicitly whitelist values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScope {
    /// Allowed org ids (empty = universe).
    #[serde(default)]
    pub orgs: Vec<String>,
    /// Allowed project ids (empty = universe).
    #[serde(default)]
    pub projects: Vec<String>,
    /// Allowed agent ids (empty = universe).
    #[serde(default)]
    pub agent_ids: Vec<String>,
}

impl AgentScope {
    /// True when no axis is restricted (universe scope).
    #[must_use]
    pub fn is_universe(&self) -> bool {
        self.orgs.is_empty() && self.projects.is_empty() && self.agent_ids.is_empty()
    }

    /// `self` contains `other` iff for each axis, either `self` is
    /// universe (empty) or every entry in `other` is present in
    /// `self`. An `other` universe can only be contained by a `self`
    /// universe on that axis.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        axis_contains(&self.orgs, &other.orgs)
            && axis_contains(&self.projects, &other.projects)
            && axis_contains(&self.agent_ids, &other.agent_ids)
    }
}

pub(crate) fn axis_contains(parent: &[String], child: &[String]) -> bool {
    if parent.is_empty() {
        return true;
    }
    if child.is_empty() {
        return false;
    }
    child.iter().all(|c| parent.iter().any(|p| p == c))
}

/// Intersect two scope axes: universe ∩ x = x; otherwise set
/// intersection preserving the first input's order.
pub(crate) fn axis_intersect(a: &[String], b: &[String]) -> Vec<String> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    a.iter().filter(|x| b.contains(x)).cloned().collect()
}

impl AgentScope {
    /// Scope intersection: per-axis intersect with universe handling.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            orgs: axis_intersect(&self.orgs, &other.orgs),
            projects: axis_intersect(&self.projects, &other.projects),
            agent_ids: axis_intersect(&self.agent_ids, &other.agent_ids),
        }
    }
}
