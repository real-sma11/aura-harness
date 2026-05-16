//! Normalized provider-agnostic conversation types.
//!
//! These are AURA canonical types for model interactions.
//! Every provider adapter maps to/from these types.

mod content;
mod content_profile;
mod message;
mod request;
mod response;
mod streaming;
mod tool;

pub use content::{ContentBlock, ImageSource, Role, ToolResultContent};
pub use content_profile::{
    ModelContentProfile, ModelContractVerdict, ModelContractViolationReason,
    ModelRequestContractViolation, ModelRequestKind, ModelRequestMetadata,
};
pub use message::Message;
pub use request::{
    MaxTokens, ModelName, ModelRequest, ModelRequestBuilder, PromptCacheRetention, Temperature,
    ThinkingConfig,
};
pub use response::{ModelResponse, ProviderTrace, StopReason, Usage};
pub use streaming::{
    AccumulatedToolUse, PartialToolUse, StreamAccumulator, StreamContentType, StreamEvent,
};
pub use tool::{CacheControl, ToolChoice, ToolDefinition};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_streaming;
