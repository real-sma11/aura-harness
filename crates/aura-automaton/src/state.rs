use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::AutomatonError;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomatonState {
    data: HashMap<String, serde_json::Value>,
}

impl AutomatonState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.data
            .get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Insert (or overwrite) the typed value `value` at `key`.
    ///
    /// Returns [`AutomatonError::StateSerialization`] when the value
    /// cannot be serialized to JSON. This used to silently drop the
    /// write — surfacing the failure typed lets callers decide
    /// whether to abort the tick or recover (Rule 4.3 — no silent
    /// error swallowing).
    pub fn set<T: Serialize>(&mut self, key: &str, value: &T) -> Result<(), AutomatonError> {
        let v =
            serde_json::to_value(value).map_err(|source| AutomatonError::StateSerialization {
                key: key.to_string(),
                source,
            })?;
        self.data.insert(key.to_string(), v);
        Ok(())
    }

    pub fn remove(&mut self, key: &str) {
        self.data.remove(key);
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.data.keys()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::AutomatonState;
    use crate::AutomatonError;

    #[test]
    fn set_round_trips_typed_value() {
        let mut state = AutomatonState::new();
        state
            .set("count", &42_u32)
            .expect("plain integer must serialize");
        assert_eq!(state.get::<u32>("count"), Some(42));
    }

    /// Failure-path coverage for the new `Result<(), AutomatonError>`
    /// signature. A `HashMap` with non-string keys cannot be serialized
    /// to a JSON object, so `serde_json::to_value` errors out — proving
    /// the write is no longer silently dropped. (`f32::NAN` is not used
    /// here because `serde_json` encodes it as `null` instead of
    /// failing, so it wouldn't exercise the error branch.)
    #[test]
    fn set_returns_state_serialization_on_unrepresentable_value() {
        let mut state = AutomatonState::new();
        let mut bad: HashMap<(u8, u8), u32> = HashMap::new();
        bad.insert((1, 2), 3);

        let result = state.set("tuple_keys", &bad);

        assert!(
            matches!(result, Err(AutomatonError::StateSerialization { ref key, .. }) if key == "tuple_keys"),
            "expected StateSerialization for unrepresentable value, got {result:?}"
        );
        assert!(
            !state.contains_key("tuple_keys"),
            "failed write must not insert a partial entry"
        );
    }
}
