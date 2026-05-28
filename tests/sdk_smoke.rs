//! Phase 9 SDK smoke test — exercises the external `aura-surface-sdk`
//! crate from the root workspace `tests/` harness so the type surface
//! is verified from an **out-of-crate** consumer perspective.
//!
//! The harness uses the documented fixture-model-provider style: the
//! [`FixtureTransport`] is a stand-in for the production transport
//! (HTTP / WebSocket / in-process). This lets the test exercise the
//! full happy path (build client → open session → send prompt →
//! receive response) without touching the network.
//!
//! Three guarantees are asserted:
//!
//! 1. `AuraClient::new` rejects an empty endpoint with the documented
//!    `SdkError::InvalidConfig` typed error.
//! 2. A round-trip prompt against the fixture transport returns the
//!    fixture-canned reply, byte-identical.
//! 3. The caller-supplied `SessionConfig::mode` is preserved on the
//!    returned `AuraSession` — the Phase 9 documented SDK input into
//!    the `AgentMode` resolution priority chain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aura_core_modes::AgentMode;
use aura_core_protocol::PROTOCOL_VERSION;
use aura_surface_sdk::{AuraClient, AuraClientConfig, SdkError, SessionConfig, SessionTransport};

/// Deterministic fixture transport. Returns a canned reply for every
/// prompt and counts the number of invocations so the test can assert
/// the session forwarded the call.
struct FixtureTransport {
    canned_reply: String,
    invocations: AtomicUsize,
}

impl FixtureTransport {
    fn new(canned_reply: impl Into<String>) -> Self {
        Self {
            canned_reply: canned_reply.into(),
            invocations: AtomicUsize::new(0),
        }
    }

    fn invocations(&self) -> usize {
        self.invocations.load(Ordering::SeqCst)
    }
}

impl SessionTransport for FixtureTransport {
    fn prompt(&self, _body: &str) -> Result<String, SdkError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(self.canned_reply.clone())
    }
}

#[test]
fn sdk_rejects_empty_endpoint() {
    let cfg = AuraClientConfig::default();
    let result = AuraClient::new(cfg, Arc::new(FixtureTransport::new("ignored")));
    let err = match result {
        Err(err) => err,
        Ok(_) => panic!("empty endpoint must be rejected"),
    };
    assert!(matches!(err, SdkError::InvalidConfig(_)));
}

#[test]
fn sdk_round_trip_prompt_against_fixture_transport() {
    let cfg = AuraClientConfig {
        endpoint: "inproc://sdk-smoke".to_string(),
        client_name: Some("phase-9-smoke".to_string()),
        protocol: PROTOCOL_VERSION,
    };
    let transport = Arc::new(FixtureTransport::new("fixture-ok"));
    let client = AuraClient::new(cfg, transport.clone()).expect("valid config builds client");

    assert_eq!(client.config().endpoint, "inproc://sdk-smoke");
    assert_eq!(
        client.config().client_name.as_deref(),
        Some("phase-9-smoke")
    );
    assert_eq!(client.config().protocol, PROTOCOL_VERSION);

    let session = client.open_session(SessionConfig {
        mode: Some(AgentMode::Plan),
        tag: Some("phase-9-smoke-session".to_string()),
    });

    assert_eq!(session.config().mode, Some(AgentMode::Plan));
    assert_eq!(
        session.config().tag.as_deref(),
        Some("phase-9-smoke-session")
    );

    let reply = session
        .prompt("hello world")
        .expect("fixture transport never errors");
    assert_eq!(reply, "fixture-ok");
    assert_eq!(transport.invocations(), 1);
}

#[test]
fn sdk_session_config_preserves_each_agent_mode_choice() {
    let cfg = AuraClientConfig {
        endpoint: "inproc://sdk-smoke".to_string(),
        ..AuraClientConfig::default()
    };
    let transport = Arc::new(FixtureTransport::new("ack"));
    let client = AuraClient::new(cfg, transport).expect("valid config");

    for mode in [
        AgentMode::Agent,
        AgentMode::Plan,
        AgentMode::Ask,
        AgentMode::Debug,
    ] {
        let session = client.open_session(SessionConfig {
            mode: Some(mode),
            tag: None,
        });
        assert_eq!(
            session.config().mode,
            Some(mode),
            "SDK SessionConfig::mode must round-trip into the open AuraSession"
        );
    }
}
