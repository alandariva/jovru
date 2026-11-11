//! MCP bearer tokens, the records that bind each one to a connection, and
//! the approval queue.
//!
//! [`token`] is the wire form `jykmcp_<id>_<secret>`: it generates, parses
//! and compares, and it does not open the vault. [`registry`] stores the
//! record in the `assistant.mcp` setting and the secret in the vault.
//! [`broker`] holds pending cards and their deadlines. [`server`] is the
//! loopback HTTP listener. None of these use GPUI. The secret is not
//! written to the setting.

mod broker;
mod registry;
mod server;
mod token;

// Fixtures using the application's global latest-write queue must not
// supersede one another's writes to their independent temporary folders.
#[cfg(test)]
static SETTINGS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub use broker::{
    ACTIVITY_LIMIT, ANSWER_NOT_SHARED, ActivityEntry, Arrival, Broker, CardContent, CardEnd,
    CardId, CardPlace, CardSnapshot, ClientResponse, ClientResult, DECISION_TIMED_OUT,
    DECISION_TIMEOUT, NOT_ACCEPTING, Outcome, QUERY_BUSY, QUERY_DECLINED, QUITTING,
    RESULT_TIMED_OUT, RunPermit, STATEMENT_TIMED_OUT, Settled, ToolName,
};
pub use registry::{
    CONNECTION_CHANGED, DEFAULT_PORT, DirectorySettings, EnabledTools, MCP_WRITES, McpSettings,
    McpSettingsWrite, ReviewPolicy, SETTINGS_KEY, TokenDraft, TokenRecord, TokenRefusal,
    TokenRegistry, VaultCleanup, environment_label, load_settings, save_settings,
};
pub use server::{CreateToken, ListenState, McpEndpoint};
pub use token::{McpToken, McpTokenId, split_wire};
