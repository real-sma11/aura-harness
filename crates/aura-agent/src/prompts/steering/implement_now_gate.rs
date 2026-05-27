//! One-shot steering when dev-loop tasks accumulate exploration tools without writes.

use std::path::PathBuf;

use super::SteeringKind;
use crate::agent_loop::{AgentLoopConfig, LoopState};

const DEFAULT_IMPLEMENT_NOW_EXPLORATION_THRESHOLD: usize = 10;
const MAX_PATHS_IN_MESSAGE: usize = 5;

/// Default exploration-tool count before the harness steers toward writes.
pub const IMPLEMENT_NOW_DEFAULT_THRESHOLD: usize = DEFAULT_IMPLEMENT_NOW_EXPLORATION_THRESHOLD;

fn implement_now_enabled() -> bool {
    !matches!(
        std::env::var("AURA_AGENT_IMPLEMENT_NOW").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

fn implement_now_threshold() -> usize {
    std::env::var("AURA_AGENT_IMPLEMENT_NOW_THRESHOLD")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_IMPLEMENT_NOW_EXPLORATION_THRESHOLD)
}

fn sample_read_paths(session_read_paths: &std::collections::HashSet<PathBuf>) -> Vec<String> {
    let mut paths: Vec<String> = session_read_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    paths.sort();
    paths.truncate(MAX_PATHS_IN_MESSAGE);
    paths
}

/// Returns [`SteeringKind::ImplementNow`] when a tracked dev-loop task has
/// crossed the exploration threshold without cumulative file writes and the
/// one-shot latch has not fired yet.
#[must_use]
pub(crate) fn evaluate_implement_now(
    config: &AgentLoopConfig,
    state: &LoopState,
) -> Option<SteeringKind> {
    if !implement_now_enabled() {
        return None;
    }
    // Tracked tasks wire `phase_reset_signal`; chat and generic callers do not.
    if config.phase_reset_signal.is_none() {
        return None;
    }
    if state.implement_now_injected {
        return None;
    }
    if state.had_any_file_write {
        return None;
    }
    let threshold = implement_now_threshold();
    if state.exploration_state.count < threshold {
        return None;
    }

    Some(SteeringKind::ImplementNow {
        exploration_count: state.exploration_state.count,
        sample_paths: sample_read_paths(&state.session_read_paths),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn task_config() -> AgentLoopConfig {
        AgentLoopConfig {
            phase_reset_signal: Some(Arc::new(AtomicBool::new(false))),
            ..AgentLoopConfig::for_agent("test-model")
        }
    }

    fn state_at_exploration(count: usize) -> LoopState {
        let config = AgentLoopConfig::for_agent("test-model");
        let mut state = LoopState::new_for_tests(&config, vec![]);
        state.exploration_state.count = count;
        state
    }

    #[test]
    fn fires_at_threshold_for_tracked_task_without_writes() {
        let config = task_config();
        let mut state = state_at_exploration(IMPLEMENT_NOW_DEFAULT_THRESHOLD);
        let kind = evaluate_implement_now(&config, &state).expect("should fire");
        match kind {
            SteeringKind::ImplementNow {
                exploration_count,
                sample_paths,
            } => {
                assert_eq!(exploration_count, IMPLEMENT_NOW_DEFAULT_THRESHOLD);
                assert!(sample_paths.is_empty());
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        state.implement_now_injected = true;
        assert!(evaluate_implement_now(&config, &state).is_none());
    }

    #[test]
    fn skipped_for_chat_without_phase_reset_signal() {
        let config = AgentLoopConfig::for_agent("test-model");
        let state = state_at_exploration(100);
        assert!(evaluate_implement_now(&config, &state).is_none());
    }

    #[test]
    fn skipped_below_threshold() {
        let config = task_config();
        let state = state_at_exploration(IMPLEMENT_NOW_DEFAULT_THRESHOLD - 1);
        assert!(evaluate_implement_now(&config, &state).is_none());
    }

    #[test]
    fn skipped_after_file_write() {
        let config = task_config();
        let mut state = state_at_exploration(100);
        state.had_any_file_write = true;
        assert!(evaluate_implement_now(&config, &state).is_none());
    }

    #[test]
    fn includes_sample_paths_sorted() {
        let config = task_config();
        let mut state = state_at_exploration(IMPLEMENT_NOW_DEFAULT_THRESHOLD);
        state
            .session_read_paths
            .insert(PathBuf::from("crates/zero-storage/src/storage.rs"));
        state
            .session_read_paths
            .insert(PathBuf::from("crates/zero-storage/src/outbox.rs"));
        let kind = evaluate_implement_now(&config, &state).unwrap();
        match kind {
            SteeringKind::ImplementNow { sample_paths, .. } => {
                assert_eq!(
                    sample_paths,
                    vec![
                        "crates/zero-storage/src/outbox.rs".to_string(),
                        "crates/zero-storage/src/storage.rs".to_string(),
                    ]
                );
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn phase_reset_signal_presence_is_sufficient() {
        let config = task_config();
        let _ = config
            .phase_reset_signal
            .as_ref()
            .unwrap()
            .load(Ordering::Relaxed);
        let state = state_at_exploration(IMPLEMENT_NOW_DEFAULT_THRESHOLD);
        assert!(evaluate_implement_now(&config, &state).is_some());
    }
}
