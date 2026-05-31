//! Runtime capability snapshot recorded through the kernel and the
//! canonical integration-matching predicates used by every layer.

use super::installed::{
    InstalledIntegrationDefinition, InstalledToolCapability, InstalledToolIntegrationRequirement,
};
use crate::types::transaction::SystemKind;
use serde::{Deserialize, Serialize};

/// Runtime capability install snapshot recorded through the kernel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilityInstall {
    pub system_kind: SystemKind,
    /// Scope that installed these capabilities (for example `session` or `automaton`).
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub installed_integrations: Vec<InstalledIntegrationDefinition>,
    #[serde(default)]
    pub installed_tools: Vec<InstalledToolCapability>,
}

impl RuntimeCapabilityInstall {
    #[must_use]
    pub fn tool_capability(&self, tool: &str) -> Option<&InstalledToolCapability> {
        self.installed_tools
            .iter()
            .find(|installed| installed.name == tool)
    }

    #[must_use]
    pub fn integration_requirement_satisfied(
        &self,
        requirement: &InstalledToolIntegrationRequirement,
    ) -> bool {
        installed_integrations_satisfy(requirement, &self.installed_integrations)
    }
}

/// Return `true` if `candidate` satisfies every populated field of `requirement`.
///
/// Each of `integration_id`, `provider`, and `kind` is matched only when
/// the corresponding `Option` on `requirement` is `Some`. A `None` field
/// acts as a wildcard. This is the canonical predicate used across the
/// kernel, runtime, and core to decide whether an installed integration
/// satisfies a tool's `required_integration`.
#[must_use]
pub fn integration_match(
    requirement: &InstalledToolIntegrationRequirement,
    candidate: &InstalledIntegrationDefinition,
) -> bool {
    requirement
        .integration_id
        .as_deref()
        .map_or(true, |expected| candidate.integration_id == expected)
        && requirement
            .provider
            .as_deref()
            .map_or(true, |expected| candidate.provider == expected)
        && requirement
            .kind
            .as_deref()
            .map_or(true, |expected| candidate.kind == expected)
}

/// Return `true` if any element of `installed` satisfies `requirement`
/// (per [`integration_match`]).
#[must_use]
pub fn installed_integrations_satisfy(
    requirement: &InstalledToolIntegrationRequirement,
    installed: &[InstalledIntegrationDefinition],
) -> bool {
    installed
        .iter()
        .any(|candidate| integration_match(requirement, candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_install(
        integration_id: &str,
        provider: &str,
        kind: &str,
    ) -> InstalledIntegrationDefinition {
        InstalledIntegrationDefinition {
            integration_id: integration_id.to_string(),
            name: integration_id.to_string(),
            provider: provider.to_string(),
            kind: kind.to_string(),
            metadata: HashMap::new(),
        }
    }

    fn req(
        integration_id: Option<&str>,
        provider: Option<&str>,
        kind: Option<&str>,
    ) -> InstalledToolIntegrationRequirement {
        InstalledToolIntegrationRequirement {
            integration_id: integration_id.map(str::to_string),
            provider: provider.map(str::to_string),
            kind: kind.map(str::to_string),
        }
    }

    #[test]
    fn empty_requirement_matches_any_candidate() {
        let candidate = make_install("any", "anyprov", "anykind");
        assert!(integration_match(&req(None, None, None), &candidate));
    }

    #[test]
    fn provider_only_requirement_matches_when_provider_matches() {
        let candidate = make_install("id-1", "github", "git");
        assert!(integration_match(
            &req(None, Some("github"), None),
            &candidate
        ));
        assert!(!integration_match(
            &req(None, Some("gitlab"), None),
            &candidate
        ));
    }

    #[test]
    fn kind_only_requirement_matches_when_kind_matches() {
        let candidate = make_install("id-1", "github", "git");
        assert!(integration_match(&req(None, None, Some("git")), &candidate));
        assert!(!integration_match(
            &req(None, None, Some("issue")),
            &candidate
        ));
    }

    #[test]
    fn integration_id_only_requirement_matches_when_id_matches() {
        let candidate = make_install("id-1", "github", "git");
        assert!(integration_match(
            &req(Some("id-1"), None, None),
            &candidate
        ));
        assert!(!integration_match(
            &req(Some("id-2"), None, None),
            &candidate
        ));
    }

    #[test]
    fn all_three_must_match_when_all_three_specified() {
        let candidate = make_install("id-1", "github", "git");
        let full = req(Some("id-1"), Some("github"), Some("git"));
        assert!(integration_match(&full, &candidate));

        let bad_id = req(Some("id-9"), Some("github"), Some("git"));
        assert!(!integration_match(&bad_id, &candidate));

        let bad_provider = req(Some("id-1"), Some("gitlab"), Some("git"));
        assert!(!integration_match(&bad_provider, &candidate));

        let bad_kind = req(Some("id-1"), Some("github"), Some("issue"));
        assert!(!integration_match(&bad_kind, &candidate));
    }

    #[test]
    fn installed_integrations_satisfy_returns_true_if_any_match() {
        let installed = vec![
            make_install("id-1", "github", "git"),
            make_install("id-2", "linear", "issue"),
        ];

        // Matches second element by provider.
        assert!(installed_integrations_satisfy(
            &req(None, Some("linear"), None),
            &installed,
        ));

        // No element provides "slack".
        assert!(!installed_integrations_satisfy(
            &req(None, Some("slack"), None),
            &installed,
        ));
    }

    #[test]
    fn installed_integrations_satisfy_empty_slice() {
        // Empty slice never satisfies anything other than the impossible
        // case where the predicate is never invoked — but `any()` on an
        // empty iterator is always `false`, regardless of requirement.
        assert!(!installed_integrations_satisfy(&req(None, None, None), &[],));
    }

    #[test]
    fn runtime_capability_install_wrapper_matches_helper() {
        let install = RuntimeCapabilityInstall {
            system_kind: SystemKind::CapabilityInstall,
            scope: "session".to_string(),
            session_id: None,
            installed_integrations: vec![make_install("id-1", "github", "git")],
            installed_tools: vec![],
        };
        let r = req(Some("id-1"), Some("github"), Some("git"));
        assert_eq!(
            install.integration_requirement_satisfied(&r),
            installed_integrations_satisfy(&r, &install.installed_integrations),
        );
    }
}
