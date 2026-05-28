//! Phase 10 — `ConnectorRegistry` last-wins integration test.
//!
//! Pins the carve-out 5c semantics: plugin-supplied connectors
//! can override an existing registration by id, and
//! `ConnectorRegistry::remove` drops a slot completely. The first-
//! active-wins surface ([`ConnectorRegistry::register`]) is
//! intentionally left in place — `replace`/`remove` are additive.

use aura_plugin_connectors::{ConnectorEntry, ConnectorRegistry};

fn entry(id: &str, plugin: &str, endpoint: &str) -> ConnectorEntry {
    ConnectorEntry {
        id: id.to_string(),
        plugin_id: plugin.to_string(),
        endpoint: endpoint.to_string(),
    }
}

#[test]
fn replace_returns_previous_and_new_entry_wins() {
    let reg = ConnectorRegistry::new();
    reg.register(entry("foo", "builtin", "https://built-in.example.com"))
        .expect("first register");

    let previous = reg.replace(entry("foo", "plugin-a", "https://plugin-a.example.com"));
    let previous = previous.expect("replace returns the prior entry");
    assert_eq!(previous.plugin_id, "builtin");
    assert_eq!(previous.endpoint, "https://built-in.example.com");

    let current = reg.get("foo").expect("get after replace");
    assert_eq!(current.plugin_id, "plugin-a");
    assert_eq!(current.endpoint, "https://plugin-a.example.com");
}

#[test]
fn replace_on_missing_id_returns_none_and_inserts() {
    let reg = ConnectorRegistry::new();
    let previous = reg.replace(entry("fresh", "plugin", "https://plugin.example.com"));
    assert!(
        previous.is_none(),
        "replacing a missing id must return None (insert semantics)"
    );
    let got = reg.get("fresh").expect("inserted entry must be readable");
    assert_eq!(got.plugin_id, "plugin");
}

#[test]
fn remove_returns_entry_and_subsequent_get_misses() {
    let reg = ConnectorRegistry::new();
    reg.register(entry("foo", "plugin", "https://plugin.example.com"))
        .expect("register");

    let removed = reg.remove("foo").expect("remove returns the entry");
    assert_eq!(removed.plugin_id, "plugin");
    assert_eq!(removed.endpoint, "https://plugin.example.com");

    assert!(
        reg.get("foo").is_err(),
        "after remove the id must miss on get"
    );
    assert!(reg.list().is_empty(), "registry must be empty after remove");
}

#[test]
fn remove_on_missing_id_is_idempotent() {
    let reg = ConnectorRegistry::new();
    assert!(reg.remove("never-registered").is_none());
    // second call must also return None — pure no-op contract.
    assert!(reg.remove("never-registered").is_none());
}
