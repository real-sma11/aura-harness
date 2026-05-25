//! Stall detection — detects when the agent is stuck repeating the same operations.

use crate::constants::STALL_STREAK_THRESHOLD;
use std::collections::HashSet;

/// Tracks write targets across iterations to detect stalls.
#[derive(Debug, Default)]
pub struct StallDetector {
    /// Previous iteration's write targets.
    prev_targets: HashSet<String>,
    /// Streak of identical write targets.
    streak: usize,
}

impl StallDetector {
    /// Update the detector with this iteration's write targets.
    ///
    /// `writes_attempted` should be true when write tools were called this
    /// iteration, even if their paths could not be extracted (e.g. empty
    /// arguments). This prevents pathless write calls from resetting the
    /// streak.
    ///
    /// Returns `true` if a stall is detected (same targets for
    /// `STALL_STREAK_THRESHOLD` iterations).
    pub fn update(
        &mut self,
        current_targets: &HashSet<String>,
        any_success: bool,
        writes_attempted: bool,
    ) -> bool {
        if any_success {
            self.streak = 0;
            self.prev_targets.clone_from(current_targets);
            return false;
        }

        if current_targets.is_empty() && !writes_attempted {
            self.streak = 0;
            self.prev_targets.clone_from(current_targets);
            return false;
        }

        // Writes were attempted but all failed (possibly with unextractable
        // paths). Treat empty targets as matching the previous set so the
        // streak keeps growing.
        if current_targets.is_empty() || *current_targets == self.prev_targets {
            self.streak += 1;
        } else {
            self.streak = 1;
            self.prev_targets.clone_from(current_targets);
        }

        self.streak >= STALL_STREAK_THRESHOLD
    }

    /// Get the current streak count.
    #[must_use]
    pub const fn streak(&self) -> usize {
        self.streak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_writes_resets() {
        let mut det = StallDetector::default();
        let empty = HashSet::new();
        assert!(!det.update(&empty, false, false));
        assert_eq!(det.streak(), 0);
    }

    #[test]
    fn test_successful_edit_resets() {
        let mut det = StallDetector::default();
        let targets: HashSet<String> = ["a.rs".to_string()].into();
        assert!(!det.update(&targets, true, true));
        assert_eq!(det.streak(), 0);
    }

    #[test]
    fn test_identical_content_increments() {
        let mut det = StallDetector::default();
        let targets: HashSet<String> = ["a.rs".to_string()].into();
        det.update(&targets, false, true);
        assert_eq!(det.streak(), 1);
        det.update(&targets, false, true);
        assert_eq!(det.streak(), 2);
    }

    #[test]
    fn test_streak_accessor() {
        let det = StallDetector::default();
        assert_eq!(det.streak(), 0);
    }

    #[test]
    fn test_no_writes_attempted_resets_streak() {
        let mut det = StallDetector::default();
        let targets: HashSet<String> = ["a.rs".to_string()].into();
        let empty = HashSet::new();
        assert!(!det.update(&targets, false, true)); // 1
        assert!(!det.update(&targets, false, true)); // 2
        assert!(!det.update(&empty, false, false)); // no writes at all: reset
        assert_eq!(det.streak(), 0);
    }
}
