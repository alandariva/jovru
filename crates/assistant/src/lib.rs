//! The model side of the AI assistant ([ADR-0028](../../../docs/adr/0028-agentic-ai-assistant-over-the-metadata-cache.md)):
//! which provider and model answer, the canonical request a round sends,
//! one streamed round at a time, what it cost and where the SQL is in an
//! answer.
//!
//! The application drives the agentic loop round by round, so it can show
//! each request before it leaves and the tool steps in between. This crate
//! knows nothing about databases, the metadata cache or the UI: what goes
//! into a request is decided by the application. The provider library is
//! private to `transport`; nothing it defines is part of this crate's API.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

// The workspace names the providers' HTTP client `reqwest-genai`, beside the
// 0.12 `reqwest` of the Databricks driver; here it is the only one.
extern crate reqwest_genai as reqwest;

mod error;
mod pricing;
mod provider;
mod request;
mod sql_blocks;
mod transport;

pub use error::classify_status;
pub use pricing::{Price, Usage};
pub use provider::{KeyUse, ProviderConfig, ProviderKind};
pub use request::{Message, ModelRequest, ToolCall, ToolSpec, estimate_tokens};
pub use sql_blocks::{SqlBlock, sql_blocks};
pub use transport::{Assistant, RoundEvent, RoundResult, Timeouts};
