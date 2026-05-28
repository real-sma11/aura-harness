//! Thread-safe in-process registry of plugin-contributed connectors.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::ConnectorError;

/// A single connector contribution. Mirrors the
/// `[[contributes.connectors]]` shape from a plugin manifest, plus
/// the contributing `plugin_id` so duplicate-id diagnostics name the
/// offending source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorEntry {
    /// Stable connector identifier (the registry merge key).
    pub id: String,
    /// Plugin id that contributed this connector.
    pub plugin_id: String,
    /// Connector endpoint string. Phase 4c does not validate the
    /// shape — the consumer (Phase 8) will decide what URL / IPC
    /// scheme contract to enforce.
    pub endpoint: String,
}

/// In-process registry. Thread-safe via an internal mutex.
#[derive(Default)]
pub struct ConnectorRegistry {
    inner: Mutex<BTreeMap<String, ConnectorEntry>>,
}

impl std::fmt::Debug for ConnectorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.inner.lock().map(|g| g.len()).unwrap_or_default();
        f.debug_struct("ConnectorRegistry")
            .field("registered", &len)
            .finish()
    }
}

impl ConnectorRegistry {
    /// Construct a new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connector contribution. Returns
    /// [`ConnectorError::AlreadyRegistered`] if another contribution
    /// already owns this id.
    ///
    /// # Errors
    ///
    /// See [`ConnectorError`].
    pub fn register(&self, entry: ConnectorEntry) -> Result<(), ConnectorError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.contains_key(&entry.id) {
            return Err(ConnectorError::AlreadyRegistered(entry.id));
        }
        guard.insert(entry.id.clone(), entry);
        Ok(())
    }

    /// Look up a connector by id.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::UnknownConnector`] for misses.
    pub fn get(&self, id: &str) -> Result<ConnectorEntry, ConnectorError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| ConnectorError::UnknownConnector(id.to_string()))
    }

    /// Enumerate every registered connector, sorted by id.
    #[must_use]
    pub fn list(&self) -> Vec<ConnectorEntry> {
        self.inner
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, plugin: &str, endpoint: &str) -> ConnectorEntry {
        ConnectorEntry {
            id: id.to_string(),
            plugin_id: plugin.to_string(),
            endpoint: endpoint.to_string(),
        }
    }

    #[test]
    fn register_then_get_returns_entry() {
        let reg = ConnectorRegistry::new();
        reg.register(entry("c1", "p1", "https://example.com"))
            .expect("first register");
        let got = reg.get("c1").expect("lookup");
        assert_eq!(got.plugin_id, "p1");
        assert_eq!(got.endpoint, "https://example.com");
    }

    #[test]
    fn duplicate_register_errors() {
        let reg = ConnectorRegistry::new();
        reg.register(entry("c1", "p1", "https://example.com"))
            .expect("first");
        let err = reg
            .register(entry("c1", "p2", "https://other.example.com"))
            .expect_err("duplicate must error");
        assert!(matches!(err, ConnectorError::AlreadyRegistered(id) if id == "c1"));
        // First-contributor-wins: the original entry is intact.
        let got = reg.get("c1").expect("lookup");
        assert_eq!(got.plugin_id, "p1");
        assert_eq!(got.endpoint, "https://example.com");
    }

    #[test]
    fn get_unknown_errors() {
        let reg = ConnectorRegistry::new();
        let err = reg.get("missing").expect_err("must miss");
        assert!(matches!(err, ConnectorError::UnknownConnector(id) if id == "missing"));
    }

    #[test]
    fn list_is_id_sorted() {
        let reg = ConnectorRegistry::new();
        reg.register(entry("zeta", "p", "z")).unwrap();
        reg.register(entry("alpha", "p", "a")).unwrap();
        reg.register(entry("mu", "p", "m")).unwrap();
        let ids: Vec<String> = reg.list().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn empty_registry_lists_nothing() {
        let reg = ConnectorRegistry::new();
        assert!(reg.list().is_empty());
    }
}
