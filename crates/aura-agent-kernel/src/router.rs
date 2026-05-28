//! Executor router for dispatching actions.

use crate::executor::{ExecuteContext, Executor};
use aura_core::{Action, Effect, EffectKind, EffectStatus};
use std::sync::Arc;
use tracing::{debug, error, instrument};

/// Router that dispatches actions to the appropriate executor.
#[derive(Clone)]
pub struct ExecutorRouter {
    executors: Vec<Arc<dyn Executor>>,
}

impl ExecutorRouter {
    /// Create a new empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            executors: Vec::new(),
        }
    }

    /// Add an executor to the router.
    pub fn add_executor(&mut self, executor: Arc<dyn Executor>) {
        self.executors.push(executor);
    }

    /// Create a router with the given executors.
    #[must_use]
    pub fn with_executors(executors: Vec<Arc<dyn Executor>>) -> Self {
        Self { executors }
    }

    /// Execute an action by finding and invoking the appropriate executor.
    ///
    /// # Routing contract
    ///
    /// A well-formed router has *at most one* executor whose
    /// [`Executor::can_handle`] returns `true` for any given action.
    /// When two or more executors match, the registry is mis-configured
    /// — silently dispatching to the first registered match was a
    /// foot-gun (Phase 6 of the system-audit refactor): the resolution
    /// depended on registration order, which is invisible at the call
    /// site.
    ///
    /// In `debug_assertions` builds (and therefore tests) this now
    /// **panics** so the misconfiguration is caught immediately. In
    /// release builds we surface a structured
    /// `EffectStatus::Failed("ambiguous executor routing: ...")`
    /// instead of letting registration order silently win.
    #[instrument(skip(self, ctx, action), fields(action_id = %action.action_id, kind = ?action.kind))]
    pub async fn execute(&self, ctx: &ExecuteContext, action: &Action) -> Effect {
        let matches: Vec<&Arc<dyn Executor>> = self
            .executors
            .iter()
            .filter(|e| e.can_handle(action))
            .collect();

        if matches.len() > 1 {
            let names: Vec<&str> = matches.iter().map(|e| e.name()).collect();
            // Always loud — operators must notice this.
            error!(
                matched = matches.len(),
                executors = ?names,
                "Multiple executors can handle action; routing is ambiguous"
            );
            debug_assert!(
                matches.len() <= 1,
                "ambiguous executor routing: {names:?} all match action {:?}",
                action.action_id
            );
            return Effect::new(
                action.action_id,
                EffectKind::Agreement,
                EffectStatus::Failed,
                format!(
                    "ambiguous executor routing: {} executors match",
                    matches.len()
                ),
            );
        }

        if let Some(executor) = matches.first() {
            debug!(executor = executor.name(), "Dispatching action to executor");
            match executor.execute(ctx, action).await {
                Ok(effect) => {
                    debug!(?effect.status, "Action executed successfully");
                    return effect;
                }
                Err(e) => {
                    error!(error = %e, "Executor failed");
                    return Effect::new(
                        action.action_id,
                        EffectKind::Agreement,
                        EffectStatus::Failed,
                        format!("Executor error: {e}"),
                    );
                }
            }
        }

        debug!("No executor found for action");
        Effect::new(
            action.action_id,
            EffectKind::Agreement,
            EffectStatus::Failed,
            "No executor available for action",
        )
    }
}

impl Default for ExecutorRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecuteContext, Executor, ExecutorError};
    use async_trait::async_trait;
    use aura_core::{ActionId, ActionKind, AgentId};

    /// Test executor that always claims to handle the action.
    struct AlwaysMatch {
        name: &'static str,
    }

    #[async_trait]
    impl Executor for AlwaysMatch {
        fn name(&self) -> &'static str {
            self.name
        }

        fn can_handle(&self, _action: &Action) -> bool {
            true
        }

        async fn execute(
            &self,
            _ctx: &ExecuteContext,
            action: &Action,
        ) -> Result<Effect, ExecutorError> {
            Ok(Effect::new(
                action.action_id,
                EffectKind::Agreement,
                EffectStatus::Committed,
                format!("ok from {}", self.name),
            ))
        }
    }

    fn dummy_action() -> Action {
        Action::new(
            ActionId::new([0xAA; 16]),
            ActionKind::Reason,
            bytes::Bytes::new(),
        )
    }

    fn dummy_ctx() -> ExecuteContext {
        ExecuteContext::new(
            AgentId::new([0u8; 32]),
            ActionId::new([0xAA; 16]),
            std::path::PathBuf::from("."),
        )
    }

    /// Two executors both match → ambiguous routing must blow up
    /// rather than silently picking the first registered match. In
    /// debug builds (which `cargo test` runs by default) the
    /// `debug_assert!` panics; that's the contract this test pins.
    #[tokio::test]
    #[should_panic(expected = "ambiguous executor routing")]
    async fn ambiguous_routing_panics_in_debug_builds() {
        let router = ExecutorRouter::with_executors(vec![
            Arc::new(AlwaysMatch { name: "first" }),
            Arc::new(AlwaysMatch { name: "second" }),
        ]);
        let _ = router.execute(&dummy_ctx(), &dummy_action()).await;
    }

    /// Single matching executor → normal dispatch.
    #[tokio::test]
    async fn single_match_dispatches_normally() {
        let router = ExecutorRouter::with_executors(vec![Arc::new(AlwaysMatch { name: "only" })]);
        let effect = router.execute(&dummy_ctx(), &dummy_action()).await;
        assert!(matches!(effect.status, EffectStatus::Committed));
    }
}
