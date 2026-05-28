//! Phase 7b: depth-exceeded rejection MUST fire BEFORE the per-parent
//! / global counters are touched, so a too-deep spawn never consumes
//! a concurrent slot.

use aura_core::AgentId;
use aura_fleet_quota::{QuotaConfig, QuotaError, QuotaPool, QuotaRequest};

#[test]
fn depth_exceeded_fires_before_concurrent_slot_is_taken() {
    let config = QuotaConfig {
        max_concurrent_per_parent: 8,
        max_concurrent_global: 64,
        max_depth: 2,
        tokens_per_minute_per_parent: None,
    };
    let pool = QuotaPool::with_config(config);
    let parent = AgentId::generate();

    let err = pool
        .try_acquire(QuotaRequest {
            agent_id: parent,
            child_depth: 3,
            max_iterations: 50,
            max_concurrent_tools: 4,
            token_budget: None,
        })
        .expect_err("depth-exceeded must reject");

    match err {
        QuotaError::DepthExceeded {
            child_depth,
            max_depth,
        } => {
            assert_eq!(child_depth, 3);
            assert_eq!(max_depth, 2);
        }
        other => panic!("expected DepthExceeded, got {other:?}"),
    }

    assert_eq!(
        pool.outstanding(),
        0,
        "depth-rejected spawn must not consume a global slot"
    );
    assert_eq!(
        pool.outstanding_for(parent),
        0,
        "depth-rejected spawn must not consume a per-parent slot"
    );
}
