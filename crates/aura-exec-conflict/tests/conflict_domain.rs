//! Smoke tests for [`ConflictRegistry`].
//!
//! Covers the three observable behaviours the rest of the exec layer
//! relies on:
//!
//! 1. Acquiring + releasing the lock for a given [`ConflictDomain`]
//!    lets the next acquire succeed immediately.
//! 2. A second concurrent acquire on the same domain blocks while the
//!    first guard is alive and surfaces [`ConflictError::Timeout`]
//!    after the wait budget elapses.
//! 3. Acquires on distinct domains run concurrently without
//!    contention.

use std::path::PathBuf;
use std::time::Duration;

use aura_exec_conflict::{ConflictDomain, ConflictError, ConflictRegistry};

#[tokio::test(flavor = "current_thread")]
async fn acquire_release_path_token() {
    let registry = ConflictRegistry::new();
    let domain = ConflictDomain::Path(PathBuf::from("/tmp/aura-exec-conflict/x"));

    {
        let _h = registry
            .acquire(domain.clone(), Duration::from_millis(100))
            .await
            .expect("first acquire must succeed");
    }

    let _h = registry
        .acquire(domain, Duration::from_millis(100))
        .await
        .expect("acquire after release must succeed");
}

#[tokio::test(flavor = "current_thread")]
async fn second_acquire_times_out_while_first_held() {
    let registry = ConflictRegistry::new();
    let domain = ConflictDomain::Path(PathBuf::from("/tmp/aura-exec-conflict/y"));

    let _held = registry
        .acquire(domain.clone(), Duration::from_millis(100))
        .await
        .expect("first acquire must succeed");

    let err = registry
        .acquire(domain.clone(), Duration::from_millis(50))
        .await
        .expect_err("contended acquire must time out");
    match err {
        ConflictError::Timeout(d) => assert_eq!(d, domain),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn distinct_domains_do_not_block_each_other() {
    let registry = ConflictRegistry::new();
    let a = ConflictDomain::Path(PathBuf::from("/tmp/aura-exec-conflict/a"));
    let b = ConflictDomain::GitOp("commit".into());

    let _h_a = registry
        .acquire(a, Duration::from_millis(50))
        .await
        .expect("first domain acquires");
    let _h_b = registry
        .acquire(b, Duration::from_millis(50))
        .await
        .expect("disjoint domain acquires without blocking");
}
