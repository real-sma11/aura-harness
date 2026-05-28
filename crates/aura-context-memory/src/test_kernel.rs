//! Shared test helpers for constructing a real `KernelModelGateway` backed
//! by an in-memory kernel + mock provider.
//!
//! Wave 2 T1 made memory components require a concrete
//! [`aura_agent::KernelModelGateway`] so every LLM call goes through the
//! kernel's append-only record log (Invariant §3). Tests that previously
//! plumbed a raw `MockProvider` now build a real kernel around it with
//! this helper.
#![allow(dead_code)]

use aura_agent::KernelModelGateway;
use aura_core::AgentId;
use aura_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_reasoner::{MockProvider, MockResponse, ModelProvider};
use aura_store::{RocksStore, Store};
use std::sync::Arc;
use tempfile::TempDir;

/// Hold-alive handles for a test kernel gateway so the caller's fixture
/// outlives the backing temp directories.
pub struct TestGateway {
    pub gateway: Arc<KernelModelGateway>,
    pub _db_dir: TempDir,
    pub _ws_dir: TempDir,
}

/// Build a `KernelModelGateway` that routes every `.complete(...)` call
/// through a dedicated kernel, which in turn talks to the supplied
/// `MockProvider`. A fresh RocksDB temp dir is used per gateway so tests
/// remain isolated.
#[must_use]
pub fn gateway_with_mock(provider: Arc<dyn ModelProvider + Send + Sync>) -> TestGateway {
    let db_dir = TempDir::new().expect("tempdir for kernel db");
    let ws_dir = TempDir::new().expect("tempdir for kernel workspace");
    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(db_dir.path(), false).expect("rocksstore open"));
    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        ..KernelConfig::default()
    };
    let kernel = Arc::new(
        Kernel::new(
            store,
            provider,
            ExecutorRouter::new(),
            config,
            AgentId::generate(),
        )
        .expect("kernel new"),
    );
    TestGateway {
        gateway: Arc::new(KernelModelGateway::new(kernel)),
        _db_dir: db_dir,
        _ws_dir: ws_dir,
    }
}

/// Convenience: gateway with an empty-text default response.
#[must_use]
pub fn gateway_default_empty() -> TestGateway {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::new().with_default_response(MockResponse::text("")));
    gateway_with_mock(provider)
}

/// Convenience: gateway whose default response is the given text.
#[must_use]
pub fn gateway_with_text(text: &str) -> TestGateway {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::new().with_default_response(MockResponse::text(text)));
    gateway_with_mock(provider)
}
