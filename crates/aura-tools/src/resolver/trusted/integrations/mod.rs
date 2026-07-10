//! Per-integration handlers — one file per provider.
//!
//! These methods back the hard-coded tool-name dispatch in
//! [`super::ToolResolver::execute_runtime_app_provider`] and (for Brave
//! Search and Resend send-email) the spec dispatch's
//! [`super::TrustedIntegrationRuntimeSpec::BraveSearch`] /
//! [`super::TrustedIntegrationRuntimeSpec::ResendSendEmail`] arms. They
//! are kept as inherent methods on `ToolResolver` so the existing
//! `self.<integration>_<verb>(...)` call sites stay intact.

mod brave;
mod github;
mod google;
mod linear;
mod notion;
mod resend;
mod slack;
