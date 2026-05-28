//! Phase 7b: per-parent concurrency cap rejects the 4th spawn when
//! `max_concurrent_per_parent = 3`.

use aura_core::AgentId;
use aura_fleet_quota::{QuotaConfig, QuotaError, QuotaPool, QuotaRequest};

fn request(parent: AgentId) -> QuotaRequest {
    QuotaRequest {
        agent_id: parent,
        child_depth: 1,
        max_iterations: 50,
        max_concurrent_tools: 4,
        token_budget: None,
    }
}

#[test]
fn fourth_spawn_rejected_when_max_per_parent_is_three() {
    let config = QuotaConfig {
        max_concurrent_per_parent: 3,
        max_concurrent_global: 1024,
        max_depth: 8,
        tokens_per_minute_per_parent: None,
    };
    let pool = QuotaPool::with_config(config);
    let parent = AgentId::generate();

    let _a = pool.try_acquire(request(parent)).expect("1st");
    let _b = pool.try_acquire(request(parent)).expect("2nd");
    let _c = pool.try_acquire(request(parent)).expect("3rd");
    let err = pool.try_acquire(request(parent)).expect_err("4th rejected");

    match err {
        QuotaError::TooManyConcurrentForParent {
            agent_id,
            current,
            max,
        } => {
            assert_eq!(agent_id, parent);
            assert_eq!(current, 3, "current must equal cap on rejection");
            assert_eq!(max, 3);
        }
        other => panic!("expected TooManyConcurrentForParent, got {other:?}"),
    }

    assert_eq!(
        pool.outstanding_for(parent),
        3,
        "rejected spawn must not bump the counter"
    );
}
