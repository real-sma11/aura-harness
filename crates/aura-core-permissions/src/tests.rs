//! Unit + property tests for the resolution math (rules §7).

use super::*;
use proptest::prelude::*;

fn scope_with_org(org: &str) -> AgentScope {
    AgentScope {
        orgs: vec![org.to_string()],
        ..AgentScope::default()
    }
}

#[test]
fn universe_scope_is_universe() {
    assert!(AgentScope::default().is_universe());
}

#[test]
fn non_universe_scope_is_not_universe() {
    assert!(!scope_with_org("a").is_universe());
}

#[test]
fn ceo_contains_empty() {
    assert!(Permissions::ceo_preset().contains(&Permissions::empty()));
}

#[test]
fn empty_does_not_contain_ceo() {
    assert!(!Permissions::empty().contains(&Permissions::ceo_preset()));
}

#[test]
fn narrower_scope_is_subset_of_wider() {
    let parent = Permissions {
        scope: AgentScope {
            orgs: vec!["a".into(), "b".into()],
            ..AgentScope::default()
        },
        capabilities: vec![Capability::SpawnAgent, Capability::ControlAgent],
    };
    let child = Permissions {
        scope: scope_with_org("a"),
        capabilities: vec![Capability::SpawnAgent],
    };
    assert!(parent.contains(&child));
}

#[test]
fn write_project_satisfies_read_project_for_same_id() {
    let held = Capability::WriteProject {
        id: "proj-1".into(),
    };
    let req = Capability::ReadProject {
        id: "proj-1".into(),
    };
    assert!(held.satisfies(&req));
}

#[test]
fn capability_serde_is_externally_tagged_camel_case() {
    let cap = Capability::ReadProject {
        id: "proj-1".into(),
    };
    let json = serde_json::to_value(&cap).unwrap();
    assert_eq!(json["type"], "readProject");
    assert_eq!(json["id"], "proj-1");
    let back: Capability = serde_json::from_value(json).unwrap();
    assert_eq!(cap, back);
}

#[test]
fn allows_tool_consults_capability() {
    let perms = Permissions::full_access();
    assert!(allows_tool(&perms, "spawn_agent"));
    assert!(allows_tool(&perms, "run_command"));
    let empty = Permissions::empty();
    assert!(!allows_tool(&empty, "spawn_agent"));
}

#[test]
fn effective_in_ask_mode_strips_spawn_agent() {
    let user = Permissions::full_access();
    let eff = effective(aura_core_modes::AgentMode::Ask, &user);
    assert!(!allows(&eff.permissions, &Capability::SpawnAgent));
    // Read capabilities should remain.
    assert!(allows(&eff.permissions, &Capability::ReadAgent));
}

#[test]
fn effective_in_agent_mode_passes_through() {
    let user = Permissions::full_access();
    let eff = effective(aura_core_modes::AgentMode::Agent, &user);
    assert!(allows(&eff.permissions, &Capability::SpawnAgent));
    assert!(allows(&eff.permissions, &Capability::ManageBilling));
}

// --- proptest strategies -------------------------------------------------

fn arb_capability() -> impl Strategy<Value = Capability> {
    prop_oneof![
        Just(Capability::SpawnAgent),
        Just(Capability::ControlAgent),
        Just(Capability::ReadAgent),
        Just(Capability::ListAgents),
        Just(Capability::ManageOrgMembers),
        Just(Capability::ManageBilling),
        Just(Capability::InvokeProcess),
        Just(Capability::PostToFeed),
        Just(Capability::GenerateMedia),
        Just(Capability::ReadAllProjects),
        Just(Capability::WriteAllProjects),
        "[a-z]{1,4}".prop_map(|id| Capability::ReadProject { id }),
        "[a-z]{1,4}".prop_map(|id| Capability::WriteProject { id }),
    ]
}

fn arb_scope() -> impl Strategy<Value = AgentScope> {
    (
        proptest::collection::vec("[a-z]{1,3}", 0..3),
        proptest::collection::vec("[a-z]{1,3}", 0..3),
        proptest::collection::vec("[a-z]{1,3}", 0..3),
    )
        .prop_map(|(orgs, projects, agent_ids)| AgentScope {
            orgs,
            projects,
            agent_ids,
        })
}

fn arb_permissions() -> impl Strategy<Value = Permissions> {
    (
        arb_scope(),
        proptest::collection::vec(arb_capability(), 0..6),
    )
        .prop_map(|(scope, capabilities)| Permissions {
            scope,
            capabilities,
        })
}

proptest! {
    #[test]
    fn prop_narrow_is_subset_of_both(p in arb_permissions(), c in arb_permissions()) {
        let n = narrow(&p, &c);
        for cap in &n.capabilities {
            prop_assert!(p.capabilities.contains(cap), "narrow lost parent membership");
            prop_assert!(c.capabilities.contains(cap), "narrow lost child membership");
        }
    }

    #[test]
    fn prop_intersect_commutative(a in arb_permissions(), b in arb_permissions()) {
        let ab = intersect(&a, &b);
        let ba = intersect(&b, &a);
        // The capability set is the same; ordering may differ.
        let mut ab_caps = ab.capabilities.clone();
        let mut ba_caps = ba.capabilities.clone();
        ab_caps.sort_by_key(|c| format!("{c:?}"));
        ba_caps.sort_by_key(|c| format!("{c:?}"));
        prop_assert_eq!(ab_caps, ba_caps);
    }

    #[test]
    fn prop_intersect_associative(
        a in arb_permissions(),
        b in arb_permissions(),
        c in arb_permissions(),
    ) {
        let lhs = intersect(&a, &intersect(&b, &c));
        let rhs = intersect(&intersect(&a, &b), &c);
        let mut l = lhs.capabilities.clone();
        let mut r = rhs.capabilities.clone();
        l.sort_by_key(|c| format!("{c:?}"));
        r.sort_by_key(|c| format!("{c:?}"));
        prop_assert_eq!(l, r);
    }

    #[test]
    fn prop_allows_narrow_implies_allows_both(
        p in arb_permissions(),
        c in arb_permissions(),
        x in arb_capability(),
    ) {
        let n = narrow(&p, &c);
        if allows(&n, &x) {
            prop_assert!(allows(&p, &x), "narrow allowed cap parent does not");
            prop_assert!(allows(&c, &x), "narrow allowed cap child does not");
        }
    }
}
