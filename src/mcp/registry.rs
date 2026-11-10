//! Records of MCP client tokens (ADR-0042).
//!
//! The secret is the 64 hex digits only. It is written to the OS vault and,
//! after a successful read, may sit in memory until the token is revoked,
//! rotated, suspended, expires, or this value is dropped. The `assistant.mcp`
//! setting stores the record without the secret. Nothing here uses GPUI,
//! binds the listening socket, or reads the clock: expiry is decided by the
//! `now_unix_ms` the caller passes.

use super::token::{McpToken, McpTokenId};
use crate::connection_appearance::Environment;
use crate::connection_store::{LatestWrite, VAULT_TIMEOUT, VaultCall, vault_within};
use crate::tr;
use jykre_core::{ConnectionId, DriverError, DriverResult, ErrorCode, SecretString, async_trait};
use jykre_secrets::{McpTokenAccount, McpTokenStore};
use jykre_storage::ConfigRepository;
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Setting key in `config.db`. There is no table of its own.
pub const SETTINGS_KEY: &str = "assistant.mcp";

/// Port used when the setting is missing, zero, or not a port.
pub const DEFAULT_PORT: u16 = 47321;

/// The only suspension reason this version writes. Any other string still
/// suspends and is kept as stored.
pub const CONNECTION_CHANGED: &str = "connection-changed";

const NAME_LIMIT: usize = 60;

/// The store's own sentence for a vault that cannot answer. A timeout uses
/// the same error, so the UI has one text for both.
const VAULT_UNAVAILABLE: &str =
    "System vault unavailable. The token could not be read, saved or removed.";

/// The writes of [`SETTINGS_KEY`]: the last one asked for wins, and an older
/// one cannot land after a newer one.
pub static MCP_WRITES: LatestWrite = LatestWrite::new();

/// Which of the three tools a token may ask for. Enabling one does not
/// approve a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnabledTools {
    pub search_objects: bool,
    pub describe_relation: bool,
    pub run_query: bool,
}

impl Default for EnabledTools {
    fn default() -> Self {
        Self {
            search_objects: true,
            describe_relation: true,
            run_query: false,
        }
    }
}

/// When a cache answer waits for the user. The spellings are the MCP
/// setting's, not the assistant's review mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReviewPolicy {
    /// Every cache answer waits.
    #[default]
    EveryCall,
    /// The first cache answer for this token since the process started.
    FirstSession,
    /// Cache answers leave without a card.
    Never,
}

impl ReviewPolicy {
    pub const ALL: [Self; 3] = [Self::EveryCall, Self::FirstSession, Self::Never];

    /// `every-call`, `first-session`, or `never`.
    pub fn key(self) -> &'static str {
        match self {
            Self::EveryCall => "every-call",
            Self::FirstSession => "first-session",
            Self::Never => "never",
        }
    }

    /// `None` when `key` is not one of [`Self::key`]'s spellings.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|policy| policy.key() == key)
    }
}

/// One token's record. The secret is not a field: the JSON is written by
/// [`McpSettings::to_value`] field by field, so a secret cannot arrive
/// through a derive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRecord {
    pub id: McpTokenId,
    pub name: String,
    pub connection_id: ConnectionId,
    pub connection_revision: i64,
    /// [`Environment::key`], or `""` when the connection has none.
    pub environment_label: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: Option<u64>,
    pub tools: EnabledTools,
    pub review: ReviewPolicy,
    pub suspended: Option<String>,
}

impl TokenRecord {
    fn from_value(value: &Value) -> Option<Self> {
        let fields = value.as_object()?;
        let id = fields
            .get("id")
            .and_then(Value::as_str)
            .and_then(McpTokenId::parse)?;
        // A connection that cannot be read cannot be bound. The object is
        // left out, as a bad id is: the record has nowhere to point.
        let connection_id = fields
            .get("connection_id")
            .and_then(Value::as_str)
            .and_then(|text| text.parse().ok())?;
        Some(Self {
            id,
            name: fields
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned(),
            connection_id,
            connection_revision: i64_field(fields.get("connection_revision"), 0),
            environment_label: fields
                .get("environment_label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            created_unix_ms: u64_field(fields.get("created_unix_ms"), 0),
            expires_unix_ms: expiry_field(fields.get("expires_unix_ms")),
            tools: tools_field(fields.get("tools")),
            review: review_field(fields.get("review")),
            suspended: suspended_field(fields.get("suspended")),
        })
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id.as_str(),
            "name": &self.name,
            "connection_id": self.connection_id.to_string(),
            "connection_revision": self.connection_revision,
            "environment_label": &self.environment_label,
            "created_unix_ms": self.created_unix_ms,
            "expires_unix_ms": self.expires_unix_ms,
            "tools": {
                "search_objects": self.tools.search_objects,
                "describe_relation": self.tools.describe_relation,
                "run_query": self.tools.run_query,
            },
            "review": self.review.key(),
            "suspended": &self.suspended,
        })
    }
}

/// `assistant.mcp`. Missing, or not an object, is the server off on
/// [`DEFAULT_PORT`] with no tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettings {
    pub enabled: bool,
    pub port: u16,
    pub tokens: Vec<TokenRecord>,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_PORT,
            tokens: Vec::new(),
        }
    }
}

impl McpSettings {
    /// Reads what [`Self::to_value`] wrote. Unknown fields are ignored. A
    /// token id that is not ten lowercase base32 characters is left out.
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(Value::Object(fields)) = value else {
            return Self::default();
        };
        let mut tokens: Vec<TokenRecord> = Vec::new();
        if let Some(values) = fields.get("tokens").and_then(Value::as_array) {
            for value in values {
                let Some(token) = TokenRecord::from_value(value) else {
                    continue;
                };
                if tokens.iter().any(|kept| kept.id == token.id) {
                    continue;
                }
                tokens.push(token);
            }
        }
        Self {
            enabled: bool_field(fields.get("enabled"), false),
            port: port_field(fields.get("port")),
            tokens,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "port": self.port,
            "tokens": self.tokens.iter().map(TokenRecord::to_value).collect::<Vec<_>>(),
        })
    }
}

/// What the caller passes to [`TokenRegistry::create`]. `token` is already
/// generated: this module does not draw random bytes, so a test can parse a
/// synthetic wire.
///
/// `token` is an [`McpToken`], which is not serializable, so this struct
/// cannot grow a JSON derive that would write the secret.
#[derive(Debug)]
pub struct TokenDraft {
    pub name: String,
    pub connection_id: ConnectionId,
    pub connection_revision: i64,
    pub environment_label: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: Option<u64>,
    pub tools: EnabledTools,
    pub review: ReviewPolicy,
    pub token: McpToken,
}

/// Why a presented secret was not accepted. The HTTP layer turns every
/// variant into the same 401. The variants stay distinct so a row can say
/// expired, suspended, or that the vault could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TokenRefusal {
    Unknown,
    Expired,
    Suspended,
    Mismatch,
    VaultUnavailable,
    MissingFromVault,
}

/// Vault entries that could not be removed after the record no longer uses
/// them. The ids are not secrets. Empty means every delete finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultCleanup {
    pub remaining: Vec<McpTokenId>,
}

/// Writes [`McpSettings`] under [`SETTINGS_KEY`]. The registry calls this
/// only after the vault write that the new record needs.
#[async_trait]
pub trait McpSettingsWrite: Send + Sync {
    async fn write_settings(&self, settings: &McpSettings) -> DriverResult<()>;
}

/// [`McpSettingsWrite`] for the data folder, through [`MCP_WRITES`].
#[derive(Debug, Clone)]
pub struct DirectorySettings {
    dir: PathBuf,
}

impl DirectorySettings {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait]
impl McpSettingsWrite for DirectorySettings {
    async fn write_settings(&self, settings: &McpSettings) -> DriverResult<()> {
        let ticket = MCP_WRITES.ask();
        save_settings(&self.dir, settings, ticket).await
    }
}

/// The environment key stored on a token: [`Environment::key`], or `""`
/// when the connection has none. Grant records use a different code for
/// "no environment"; this is not that code, and it is not the word
/// `Production`.
pub fn environment_label(environment: Option<Environment>) -> &'static str {
    match environment {
        Some(environment) => environment.key(),
        None => "",
    }
}

/// Reads [`SETTINGS_KEY`]. A missing setting is [`McpSettings::default`].
pub async fn load_settings(dir: &Path) -> DriverResult<McpSettings> {
    let repository = ConfigRepository::open(dir).await?;
    let value = repository.setting(SETTINGS_KEY).await;
    repository.close().await;
    Ok(McpSettings::from_value(value?.as_ref()))
}

/// Saves [`SETTINGS_KEY`] for `ticket` of [`MCP_WRITES`]. A ticket that a
/// newer ask superseded is skipped, which is success: the newer value is
/// the one that lands.
pub async fn save_settings(dir: &Path, settings: &McpSettings, ticket: u64) -> DriverResult<()> {
    MCP_WRITES
        .run(ticket, async {
            let repository = ConfigRepository::open(dir).await?;
            let result = repository
                .set_setting(SETTINGS_KEY, &settings.to_value())
                .await;
            repository.close().await;
            result
        })
        .await
        .unwrap_or(Ok(()))
}

/// The records, the vault, and the secrets read during this process.
///
/// `S` is the vault. It has no timeout of its own; every call is limited
/// to [`VAULT_TIMEOUT`]. `W` writes the setting.
pub struct TokenRegistry<S, W = DirectorySettings> {
    store: S,
    writer: W,
    settings: McpSettings,
    /// Secret parts only, keyed by id. Debug prints how many there are, not
    /// the map: the values are secrets.
    secrets: HashMap<McpTokenId, SecretString>,
    pending_vault_deletions: Vec<McpTokenId>,
    vault_limit: Duration,
}

impl<S, W> fmt::Debug for TokenRegistry<S, W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenRegistry")
            .field("settings", &self.settings)
            .field("pending_vault_deletions", &self.pending_vault_deletions)
            .field("cached_secrets", &self.secrets.len())
            .finish()
    }
}

impl<S: McpTokenStore> TokenRegistry<S, DirectorySettings> {
    /// Loads the setting from `dir`. A missing setting is the default, not
    /// an error.
    pub async fn open(dir: impl Into<PathBuf>, store: S) -> DriverResult<Self> {
        let dir = dir.into();
        let settings = load_settings(&dir).await?;
        Ok(Self::new(store, DirectorySettings::new(dir), settings))
    }
}

impl<S, W> TokenRegistry<S, W>
where
    S: McpTokenStore,
    W: McpSettingsWrite,
{
    pub fn new(store: S, writer: W, settings: McpSettings) -> Self {
        Self {
            store,
            writer,
            settings,
            secrets: HashMap::new(),
            pending_vault_deletions: Vec::new(),
            vault_limit: VAULT_TIMEOUT,
        }
    }

    pub fn settings(&self) -> &McpSettings {
        &self.settings
    }

    pub fn record(&self, id: &McpTokenId) -> Option<&TokenRecord> {
        self.settings.tokens.iter().find(|token| token.id == *id)
    }

    /// Ids whose vault entries are still there after the record dropped
    /// them. Not written to the setting. The UI retries with
    /// [`Self::retry_vault_deletions`].
    pub fn pending_vault_deletions(&self) -> &[McpTokenId] {
        &self.pending_vault_deletions
    }

    /// Validates the draft, writes the secret part to the vault, then saves
    /// the record. If the vault write fails, nothing is saved. If the save
    /// fails, the vault entry is removed; a failed removal is remembered
    /// and the error is still the save failure.
    pub async fn create(&mut self, draft: TokenDraft) -> DriverResult<()> {
        let name = validate_name(&draft.name, &self.settings.tokens, None)?;
        validate_environment(&draft.environment_label)?;
        let id = *draft.token.id();
        if self.settings.tokens.iter().any(|token| token.id == id) {
            return Err(duplicate_token());
        }
        let Some(account) = account_of(&id) else {
            return Err(bad_account());
        };
        let secret = SecretString::from(draft.token.expose_secret().to_owned());
        let limit = self.vault_limit;
        Self::limit_call(limit, "set", self.store.set(account, secret)).await?;
        let mut next = self.settings.clone();
        next.tokens.push(TokenRecord {
            id,
            name,
            connection_id: draft.connection_id,
            connection_revision: draft.connection_revision,
            environment_label: draft.environment_label,
            created_unix_ms: draft.created_unix_ms,
            expires_unix_ms: draft.expires_unix_ms,
            tools: draft.tools,
            review: draft.review,
            suspended: None,
        });
        if let Err(error) = self.store_settings(next).await {
            self.remember_if_vault_remains(id).await;
            return Err(error);
        }
        Ok(())
    }

    /// Keeps the name, connection, tools, review, expiry, and suspension.
    /// The new secret is written, the record swaps, then the old vault
    /// entry is removed. The old id stops matching as soon as the record
    /// swaps, even when that removal fails.
    ///
    /// A failed write of the new secret leaves the old record. A failed
    /// save removes the new vault entry and leaves the old record. A failed
    /// removal of the old entry keeps the new record and lists the old id
    /// in [`VaultCleanup::remaining`].
    pub async fn rotate(&mut self, id: &McpTokenId, token: McpToken) -> DriverResult<VaultCleanup> {
        let Some(index) = self
            .settings
            .tokens
            .iter()
            .position(|record| record.id == *id)
        else {
            return Err(missing_token());
        };
        let new_id = *token.id();
        if new_id == *id {
            return Err(DriverError::validation(tr!("Rotate needs a new token.")));
        }
        if self.settings.tokens.iter().any(|token| token.id == new_id) {
            return Err(duplicate_token());
        }
        let Some(account) = account_of(&new_id) else {
            return Err(bad_account());
        };
        let secret = SecretString::from(token.expose_secret().to_owned());
        let limit = self.vault_limit;
        Self::limit_call(limit, "set", self.store.set(account, secret)).await?;
        let mut next = self.settings.clone();
        let Some(record) = next.tokens.get_mut(index) else {
            // `index` came from this list. If it is gone, the new vault
            // entry must not be left behind.
            self.remember_if_vault_remains(new_id).await;
            return Err(missing_token());
        };
        record.id = new_id;
        if let Err(error) = self.store_settings(next).await {
            self.remember_if_vault_remains(new_id).await;
            return Err(error);
        }
        self.secrets.remove(id);
        Ok(self.forget_vault(*id).await)
    }

    /// Drops the record and the cached secret, then the vault entry. The
    /// token stops even when the vault delete fails; that id is then in
    /// [`VaultCleanup::remaining`] and in [`Self::pending_vault_deletions`].
    pub async fn revoke(&mut self, id: &McpTokenId) -> DriverResult<VaultCleanup> {
        if self.record(id).is_none() {
            return Err(missing_token());
        }
        let mut next = self.settings.clone();
        next.tokens.retain(|token| token.id != *id);
        self.store_settings(next).await?;
        self.secrets.remove(id);
        Ok(self.forget_vault(*id).await)
    }

    /// Revokes every token bound to `connection_id`, with the same vault
    /// rule as [`Self::revoke`]. Tokens of other connections stay.
    pub async fn delete_connection(
        &mut self,
        connection_id: ConnectionId,
    ) -> DriverResult<VaultCleanup> {
        let removed: Vec<McpTokenId> = self
            .settings
            .tokens
            .iter()
            .filter(|token| token.connection_id == connection_id)
            .map(|token| token.id)
            .collect();
        if removed.is_empty() {
            return Ok(VaultCleanup {
                remaining: Vec::new(),
            });
        }
        let mut next = self.settings.clone();
        next.tokens
            .retain(|token| token.connection_id != connection_id);
        self.store_settings(next).await?;
        let mut remaining = Vec::new();
        for id in removed {
            self.secrets.remove(&id);
            remaining.extend(self.forget_vault(id).await.remaining);
        }
        Ok(VaultCleanup { remaining })
    }

    /// Suspends tokens of `connection_id` whose stored revision or
    /// environment key differs from the live profile. A token already
    /// suspended for another reason is left as stored. `connection-changed`
    /// is written only when the token was not suspended. Other connections
    /// stay. A matching profile does not clear a suspension; [`Self::confirm`]
    /// does.
    pub async fn note_profile(
        &mut self,
        connection_id: ConnectionId,
        revision: i64,
        environment_key: &str,
    ) -> DriverResult<()> {
        let mut next = self.settings.clone();
        let mut suspended = Vec::new();
        for token in &mut next.tokens {
            if token.connection_id != connection_id {
                continue;
            }
            let differs =
                token.connection_revision != revision || token.environment_label != environment_key;
            if !differs || token.suspended.is_some() {
                continue;
            }
            token.suspended = Some(CONNECTION_CHANGED.to_owned());
            suspended.push(token.id);
        }
        if suspended.is_empty() {
            return Ok(());
        }
        self.store_settings(next).await?;
        for id in suspended {
            self.secrets.remove(&id);
        }
        Ok(())
    }

    /// Clears a missing suspension or `connection-changed`, and stores the
    /// revision and environment key the caller passes. A preserved reason
    /// stays, and the error says so without repeating that reason.
    pub async fn confirm(
        &mut self,
        id: &McpTokenId,
        revision: i64,
        environment_key: &str,
    ) -> DriverResult<()> {
        let Some(token) = self.record(id) else {
            return Err(missing_token());
        };
        if matches!(token.suspended.as_deref(), Some(reason) if reason != CONNECTION_CHANGED) {
            return Err(DriverError::validation(tr!(
                "This token stays suspended. This version cannot clear that reason."
            )));
        }
        validate_environment(environment_key)?;
        let environment_key = environment_key.to_owned();
        self.update(id, |token| {
            token.suspended = None;
            token.connection_revision = revision;
            token.environment_label = environment_key;
        })
        .await
    }

    pub async fn set_tools(&mut self, id: &McpTokenId, tools: EnabledTools) -> DriverResult<()> {
        self.update(id, |token| token.tools = tools).await
    }

    pub async fn set_review(&mut self, id: &McpTokenId, review: ReviewPolicy) -> DriverResult<()> {
        self.update(id, |token| token.review = review).await
    }

    pub async fn set_name(&mut self, id: &McpTokenId, name: &str) -> DriverResult<()> {
        if self.record(id).is_none() {
            return Err(missing_token());
        }
        let name = validate_name(name, &self.settings.tokens, Some(*id))?;
        self.update(id, |token| token.name = name).await
    }

    /// `confirm` is false when the tab is shortening the expiry: a later
    /// instant, or a change from a finite expiry to never, is rejected.
    /// `confirm` is true when the user picked a new choice, including a
    /// later one or never. The same instant is always accepted.
    pub async fn set_expiry(
        &mut self,
        id: &McpTokenId,
        expires: Option<u64>,
        confirm: bool,
    ) -> DriverResult<()> {
        let Some(current) = self.record(id).map(|token| token.expires_unix_ms) else {
            return Err(missing_token());
        };
        if !confirm && expiry_moves_later(current, expires) {
            return Err(DriverError::validation(tr!(
                "Choose an earlier expiry, or confirm this one."
            )));
        }
        self.update(id, |token| token.expires_unix_ms = expires)
            .await
    }

    pub async fn set_enabled(&mut self, enabled: bool) -> DriverResult<()> {
        let mut next = self.settings.clone();
        next.enabled = enabled;
        self.store_settings(next).await
    }

    /// Port 0 is rejected. Binding the socket is not this module.
    pub async fn set_port(&mut self, port: u16) -> DriverResult<()> {
        if port == 0 {
            return Err(DriverError::validation(tr!(
                "Enter a port between 1 and 65535."
            )));
        }
        let mut next = self.settings.clone();
        next.port = port;
        self.store_settings(next).await
    }

    /// `Ok(())` when `presented` is the secret part of a live token.
    /// `presented` is the 64 hex digits, not the wire.
    ///
    /// The first successful vault read is cached. Expiry is `now_unix_ms >=
    /// expires_unix_ms`. This function does not read the clock. A bad id is
    /// not a vault call: only [`McpTokenAccount::parse`] builds the account.
    pub async fn secret_matches(
        &mut self,
        id: &McpTokenId,
        presented: &str,
        now_unix_ms: u64,
    ) -> Result<(), TokenRefusal> {
        let found = self
            .settings
            .tokens
            .iter()
            .find(|token| token.id == *id)
            .map(|token| (token.expires_unix_ms, token.suspended.is_some()));
        let Some((expires, suspended)) = found else {
            return Err(TokenRefusal::Unknown);
        };
        // At the stored instant the token has expired. Earlier is still live.
        if expires.is_some_and(|expires| now_unix_ms >= expires) {
            self.secrets.remove(id);
            return Err(TokenRefusal::Expired);
        }
        if suspended {
            self.secrets.remove(id);
            return Err(TokenRefusal::Suspended);
        }
        if let Some(cached) = self.secrets.get(id) {
            let matched = secret_eq(cached.expose_secret(), presented);
            return if matched {
                Ok(())
            } else {
                Err(TokenRefusal::Mismatch)
            };
        }
        // The id type and the account use the same alphabet. A failure here
        // is not sent to the vault.
        let Some(account) = account_of(id) else {
            return Err(TokenRefusal::Unknown);
        };
        let limit = self.vault_limit;
        let secret = match Self::limit_call(limit, "get", self.store.get(account)).await {
            Ok(Some(secret)) => secret,
            Ok(None) => return Err(TokenRefusal::MissingFromVault),
            Err(_) => return Err(TokenRefusal::VaultUnavailable),
        };
        let matched = secret_eq(secret.expose_secret(), presented);
        self.secrets.insert(*id, secret);
        if matched {
            Ok(())
        } else {
            Err(TokenRefusal::Mismatch)
        }
    }

    /// Tries [`Self::pending_vault_deletions`] again. Ids that still fail
    /// stay on the list. The error is the vault's, without those ids; the
    /// list is already updated when it is returned.
    pub async fn retry_vault_deletions(&mut self) -> DriverResult<VaultCleanup> {
        let pending = std::mem::take(&mut self.pending_vault_deletions);
        let mut remaining = Vec::new();
        let mut failure = None;
        for id in pending {
            if let Err(error) = self.delete_vault(id).await {
                remaining.push(id);
                failure = Some(error);
            }
        }
        self.pending_vault_deletions.clone_from(&remaining);
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(VaultCleanup { remaining })
    }

    async fn update(
        &mut self,
        id: &McpTokenId,
        change: impl FnOnce(&mut TokenRecord),
    ) -> DriverResult<()> {
        if self.record(id).is_none() {
            return Err(missing_token());
        }
        let mut next = self.settings.clone();
        let Some(token) = next.tokens.iter_mut().find(|token| token.id == *id) else {
            return Err(missing_token());
        };
        change(token);
        self.store_settings(next).await
    }

    async fn store_settings(&mut self, settings: McpSettings) -> DriverResult<()> {
        if settings == self.settings {
            return Ok(());
        }
        self.writer.write_settings(&settings).await?;
        self.settings = settings;
        Ok(())
    }

    /// The record no longer names `id`. A vault failure is not this
    /// function's error: the caller already committed the record, and the
    /// id would have to be copied into the message. The cleanup value is
    /// what the UI shows, and the id waits for [`Self::retry_vault_deletions`].
    async fn forget_vault(&mut self, id: McpTokenId) -> VaultCleanup {
        if self.delete_vault(id).await.is_err() {
            self.remember(id);
            return VaultCleanup {
                remaining: vec![id],
            };
        }
        VaultCleanup {
            remaining: Vec::new(),
        }
    }

    /// Removes a vault entry written for a record that was not saved. The
    /// save error is what the caller returns. A failed removal is only
    /// remembered.
    async fn remember_if_vault_remains(&mut self, id: McpTokenId) {
        if self.delete_vault(id).await.is_err() {
            self.remember(id);
        }
    }

    async fn delete_vault(&self, id: McpTokenId) -> DriverResult<()> {
        let Some(account) = account_of(&id) else {
            return Err(bad_account());
        };
        let limit = self.vault_limit;
        Self::limit_call(limit, "delete", self.store.delete(account)).await
    }

    fn remember(&mut self, id: McpTokenId) {
        if !self.pending_vault_deletions.contains(&id) {
            self.pending_vault_deletions.push(id);
        }
    }

    async fn limit_call<T>(
        limit: Duration,
        operation: &'static str,
        work: impl Future<Output = DriverResult<T>>,
    ) -> DriverResult<T> {
        match vault_within(VaultCall::new("mcp_token", operation), limit, work).await {
            Err(error) if error.code == ErrorCode::Timeout => Err(vault_unavailable()),
            other => other,
        }
    }

    /// Shortens the vault limit. Production [`Self::new`] uses
    /// [`VAULT_TIMEOUT`]; tests must not wait that out.
    #[cfg(test)]
    fn with_vault_limit(mut self, limit: Duration) -> Self {
        self.vault_limit = limit;
        self
    }

    #[cfg(test)]
    fn vault_limit(&self) -> Duration {
        self.vault_limit
    }
}

/// [`McpTokenAccount::parse`] is the only constructor. The token id uses the
/// same alphabet, so a stored id parses. Anything else is not a vault call.
fn account_of(id: &McpTokenId) -> Option<McpTokenAccount> {
    McpTokenAccount::parse(id.as_str())
}

fn vault_unavailable() -> DriverError {
    DriverError::new(ErrorCode::Authentication, VAULT_UNAVAILABLE)
}

fn missing_token() -> DriverError {
    DriverError::new(
        ErrorCode::NotFound,
        tr!("This token is no longer in the list."),
    )
}

fn duplicate_token() -> DriverError {
    DriverError::validation(tr!("This token is already in the list."))
}

fn bad_account() -> DriverError {
    DriverError::validation(tr!("This token's id cannot name a vault account."))
}

fn validate_name(
    name: &str,
    tokens: &[TokenRecord],
    except: Option<McpTokenId>,
) -> DriverResult<String> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 {
        return Err(DriverError::validation(tr!("Enter a name for the token.")));
    }
    if length > NAME_LIMIT {
        return Err(DriverError::validation(tr!(
            "Enter a name of at most 60 characters."
        )));
    }
    if tokens
        .iter()
        .any(|token| token.name == name && except != Some(token.id))
    {
        return Err(DriverError::validation(tr!(
            "A token already has this name."
        )));
    }
    Ok(name.to_owned())
}

fn validate_environment(label: &str) -> DriverResult<()> {
    if label.is_empty() || Environment::ALL.iter().any(|item| item.key() == label) {
        Ok(())
    } else {
        Err(DriverError::validation(tr!(
            "The environment label has to be development, staging, production, or empty."
        )))
    }
}

/// A later instant, or dropping a finite expiry, needs the caller to confirm.
/// Setting an expiry on a token that had none is shorter, so it does not.
fn expiry_moves_later(current: Option<u64>, next: Option<u64>) -> bool {
    match (current, next) {
        (Some(current), Some(next)) => next > current,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Same fold as [`McpToken::secret_matches`]. Lengths are compared first, as
/// there. The stored value is the secret part, so the wire is not rebuilt.
fn secret_eq(stored: &str, presented: &str) -> bool {
    let stored = stored.as_bytes();
    let presented = presented.as_bytes();
    stored.len() == presented.len()
        && stored
            .iter()
            .zip(presented)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

fn bool_field(value: Option<&Value>, default: bool) -> bool {
    value.and_then(Value::as_bool).unwrap_or(default)
}

fn u64_field(value: Option<&Value>, default: u64) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(default)
}

fn i64_field(value: Option<&Value>, default: i64) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(default)
}

fn port_field(value: Option<&Value>) -> u16 {
    let Some(port) = value.and_then(Value::as_u64) else {
        return DEFAULT_PORT;
    };
    match u16::try_from(port) {
        Ok(port) if port != 0 => port,
        _ => DEFAULT_PORT,
    }
}

fn expiry_field(value: Option<&Value>) -> Option<u64> {
    match value {
        None | Some(Value::Null) => None,
        Some(other) => other.as_u64(),
    }
}

fn review_field(value: Option<&Value>) -> ReviewPolicy {
    value
        .and_then(Value::as_str)
        .and_then(ReviewPolicy::from_key)
        .unwrap_or_default()
}

fn tools_field(value: Option<&Value>) -> EnabledTools {
    let Some(fields) = value.and_then(Value::as_object) else {
        return EnabledTools::default();
    };
    EnabledTools {
        search_objects: bool_field(fields.get("search_objects"), true),
        describe_relation: bool_field(fields.get("describe_relation"), true),
        run_query: bool_field(fields.get("run_query"), false),
    }
}

/// A string suspends, including `""` and reasons this version does not
/// write. Null, a missing field, and any other JSON type do not.
fn suspended_field(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(reason)) => Some(reason.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_appearance::Environment;
    use crate::connection_store::VAULT_TIMEOUT;
    use crate::mcp::{McpToken, McpTokenId};
    use jykre_core::{
        ConnectionId, DriverError, DriverResult, ErrorCode, SecretString, async_trait,
    };
    use jykre_secrets::McpTokenStore;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const ID_A: &str = "abcdefghij";
    const ID_B: &str = "bbbbbbbbbb";
    const ID_C: &str = "234567abcd";
    const VAULT_SENTENCE: &str =
        "System vault unavailable. The token could not be read, saved or removed.";
    const SAVE_SENTENCE: &str = "The token record could not be saved.";

    fn suspended_of<S, W>(registry: &TokenRegistry<S, W>, id: &str) -> Option<String>
    where
        S: McpTokenStore,
        W: McpSettingsWrite,
    {
        registry
            .record(&McpTokenId::parse(id).unwrap())
            .unwrap()
            .suspended
            .clone()
    }

    fn conn(n: u8) -> ConnectionId {
        format!("00000000-0000-4000-8000-{n:012x}").parse().unwrap()
    }

    fn mcp_token(id: &str, fill: &str) -> McpToken {
        McpToken::parse(&format!("jykmcp_{id}_{}", fill.repeat(32))).unwrap()
    }

    fn draft_named(token: McpToken, name: &str) -> TokenDraft {
        TokenDraft {
            name: name.to_owned(),
            connection_id: conn(1),
            connection_revision: 4,
            environment_label: "production".to_owned(),
            created_unix_ms: 1_700_000_000_000,
            expires_unix_ms: Some(1_700_000_000_000 + 86_400_000),
            tools: EnabledTools::default(),
            review: ReviewPolicy::EveryCall,
            token,
        }
    }

    fn draft(token: McpToken) -> TokenDraft {
        draft_named(token, "Cursor on laptop")
    }

    fn record(
        id: &str,
        connection: ConnectionId,
        revision: i64,
        environment: &str,
        suspended: Option<&str>,
    ) -> TokenRecord {
        TokenRecord {
            id: McpTokenId::parse(id).unwrap(),
            name: "Cursor".to_owned(),
            connection_id: connection,
            connection_revision: revision,
            environment_label: environment.to_owned(),
            created_unix_ms: 10,
            expires_unix_ms: Some(1_000),
            tools: EnabledTools::default(),
            review: ReviewPolicy::EveryCall,
            suspended: suspended.map(str::to_owned),
        }
    }

    fn harness(
        settings: McpSettings,
    ) -> (
        TokenRegistry<FakeVault, MemorySettings>,
        FakeVault,
        MemorySettings,
    ) {
        let vault = FakeVault::default();
        let writer = MemorySettings::default();
        let registry = TokenRegistry::new(vault.clone(), writer.clone(), settings);
        (registry, vault, writer)
    }

    fn assert_no_secret(error: &DriverError, secret: &str, id: &str) {
        assert!(!error.message.contains(secret), "{}", error.message);
        assert!(!error.message.contains(id), "{}", error.message);
        let rendered = format!("{error:?}");
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains(id));
    }

    #[derive(Clone, Default)]
    struct FakeVault {
        inner: Arc<FakeVaultInner>,
    }

    struct FakeVaultInner {
        entries: Mutex<BTreeMap<String, String>>,
        fail_set: AtomicBool,
        fail_delete: AtomicBool,
        fail_get: AtomicBool,
        hang: AtomicBool,
        gets: AtomicUsize,
        sets: AtomicUsize,
        deletes: AtomicUsize,
    }

    impl Default for FakeVaultInner {
        fn default() -> Self {
            Self {
                entries: Mutex::new(BTreeMap::new()),
                fail_set: AtomicBool::new(false),
                fail_delete: AtomicBool::new(false),
                fail_get: AtomicBool::new(false),
                hang: AtomicBool::new(false),
                gets: AtomicUsize::new(0),
                sets: AtomicUsize::new(0),
                deletes: AtomicUsize::new(0),
            }
        }
    }

    impl FakeVault {
        fn fail_set(&self, fail: bool) {
            self.inner.fail_set.store(fail, Ordering::SeqCst);
        }

        fn fail_delete(&self, fail: bool) {
            self.inner.fail_delete.store(fail, Ordering::SeqCst);
        }

        fn fail_get(&self, fail: bool) {
            self.inner.fail_get.store(fail, Ordering::SeqCst);
        }

        fn hang(&self, hang: bool) {
            self.inner.hang.store(hang, Ordering::SeqCst);
        }

        fn gets(&self) -> usize {
            self.inner.gets.load(Ordering::SeqCst)
        }

        fn sets(&self) -> usize {
            self.inner.sets.load(Ordering::SeqCst)
        }

        fn deletes(&self) -> usize {
            self.inner.deletes.load(Ordering::SeqCst)
        }

        fn secret_of(&self, id: &str) -> Option<String> {
            self.inner.entries.lock().unwrap().get(id).cloned()
        }

        fn plant(&self, id: &str, secret: &str) {
            self.inner
                .entries
                .lock()
                .unwrap()
                .insert(id.to_owned(), secret.to_owned());
        }
    }

    #[async_trait]
    impl McpTokenStore for FakeVault {
        async fn get(&self, account: McpTokenAccount) -> DriverResult<Option<SecretString>> {
            self.inner.gets.fetch_add(1, Ordering::SeqCst);
            if self.inner.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.inner.fail_get.load(Ordering::SeqCst) {
                return Err(vault_unavailable());
            }
            let secret = self
                .inner
                .entries
                .lock()
                .unwrap()
                .get(account.id())
                .cloned();
            Ok(secret.map(SecretString::from))
        }

        async fn set(&self, account: McpTokenAccount, token: SecretString) -> DriverResult<()> {
            self.inner.sets.fetch_add(1, Ordering::SeqCst);
            if self.inner.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.inner.fail_set.load(Ordering::SeqCst) {
                return Err(vault_unavailable());
            }
            self.inner
                .entries
                .lock()
                .unwrap()
                .insert(account.id().to_owned(), token.expose_secret().to_owned());
            Ok(())
        }

        async fn delete(&self, account: McpTokenAccount) -> DriverResult<()> {
            self.inner.deletes.fetch_add(1, Ordering::SeqCst);
            if self.inner.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.inner.fail_delete.load(Ordering::SeqCst) {
                return Err(vault_unavailable());
            }
            self.inner.entries.lock().unwrap().remove(account.id());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MemorySettings {
        inner: Arc<MemorySettingsInner>,
    }

    struct MemorySettingsInner {
        saved: Mutex<Option<McpSettings>>,
        fail: AtomicBool,
        writes: AtomicUsize,
    }

    impl Default for MemorySettingsInner {
        fn default() -> Self {
            Self {
                saved: Mutex::new(None),
                fail: AtomicBool::new(false),
                writes: AtomicUsize::new(0),
            }
        }
    }

    impl MemorySettings {
        fn fail(&self, fail: bool) {
            self.inner.fail.store(fail, Ordering::SeqCst);
        }

        fn writes(&self) -> usize {
            self.inner.writes.load(Ordering::SeqCst)
        }

        fn saved(&self) -> Option<McpSettings> {
            self.inner.saved.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl McpSettingsWrite for MemorySettings {
        async fn write_settings(&self, settings: &McpSettings) -> DriverResult<()> {
            self.inner.writes.fetch_add(1, Ordering::SeqCst);
            if self.inner.fail.load(Ordering::SeqCst) {
                return Err(DriverError::new(ErrorCode::Database, SAVE_SENTENCE));
            }
            *self.inner.saved.lock().unwrap() = Some(settings.clone());
            Ok(())
        }
    }

    struct RemoveDir(std::path::PathBuf);

    impl Drop for RemoveDir {
        fn drop(&mut self) {
            // The scratch folder is under the temp directory. A failed
            // cleanup does not change what the test asserted.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn from_value_drops_a_bad_id_preserves_unknown_suspension_and_omits_a_secret() {
        let secret = "ab".repeat(32);
        let wire = format!("jykmcp_{ID_A}_{secret}");
        let connection = "11111111-1111-4111-8111-111111111111";
        let value = json!({
            "enabled": true,
            "port": 0,
            "secret": secret,
            "wire": wire,
            "ignored": 1,
            "tokens": [
                {
                    "id": ID_A,
                    "name": "  Cursor on laptop  ",
                    "connection_id": connection,
                    "connection_revision": 7,
                    "environment_label": "production",
                    "created_unix_ms": 10,
                    "expires_unix_ms": null,
                    "tools": {"search_objects": false, "extra": true},
                    "review": "nope",
                    "suspended": "later-version",
                    "secret": secret,
                    "wire": wire
                },
                {
                    "id": ID_A,
                    "name": "duplicate",
                    "connection_id": connection,
                    "secret": secret
                },
                {"id": "ABCDEFGHIJ", "name": "upper", "connection_id": connection, "secret": secret},
                {"id": "abcdefghi0", "name": "digit", "connection_id": connection},
                {"id": "short", "name": "short", "connection_id": connection},
                {"id": ID_B, "name": "bad connection", "connection_id": "not-a-uuid"},
                1,
                {
                    "id": ID_C,
                    "name": "second",
                    "connection_id": connection,
                    "environment_label": "Production",
                    "review": "first-session"
                }
            ]
        });
        let settings = McpSettings::from_value(Some(&value));
        assert!(settings.enabled);
        assert_eq!(settings.port, DEFAULT_PORT);
        assert_eq!(settings.tokens.len(), 2);
        let first = &settings.tokens[0];
        assert_eq!(first.id.as_str(), ID_A);
        assert_eq!(first.name, "Cursor on laptop");
        assert_eq!(first.connection_id.to_string(), connection);
        assert_eq!(first.connection_revision, 7);
        assert_eq!(first.environment_label, "production");
        assert_eq!(first.created_unix_ms, 10);
        assert_eq!(first.expires_unix_ms, None);
        assert!(!first.tools.search_objects);
        assert!(first.tools.describe_relation);
        assert!(!first.tools.run_query);
        assert_eq!(first.review, ReviewPolicy::EveryCall);
        assert_eq!(first.suspended.as_deref(), Some("later-version"));
        let second = &settings.tokens[1];
        assert_eq!(second.id.as_str(), ID_C);
        assert_eq!(second.review, ReviewPolicy::FirstSession);
        assert_eq!(second.suspended, None);
        assert_eq!(second.expires_unix_ms, None);
        assert_eq!(second.environment_label, "Production");
        assert_eq!(second.tools, EnabledTools::default());

        let stored = settings.to_value();
        let text = stored.to_string();
        assert!(!text.contains(&secret));
        assert!(!text.contains("jykmcp_"));
        assert!(!text.contains("\"secret\""));
        assert_eq!(stored["tokens"][0]["suspended"], "later-version");
        assert!(stored["tokens"][0].get("secret").is_none());
        assert_eq!(McpSettings::from_value(Some(&stored)), settings);
    }

    #[test]
    fn a_missing_or_non_object_setting_is_off_on_the_default_port() {
        for value in [
            None,
            Some(json!(null)),
            Some(json!([])),
            Some(json!(true)),
            Some(json!("x")),
        ] {
            assert_eq!(
                McpSettings::from_value(value.as_ref()),
                McpSettings::default()
            );
        }
        assert_eq!(
            McpSettings::from_value(Some(&json!({}))),
            McpSettings::default()
        );
        for port in [
            json!(0),
            json!(65536),
            json!(-1),
            json!("47321"),
            json!(null),
        ] {
            let settings = McpSettings::from_value(Some(&json!({"port": port})));
            assert_eq!(settings.port, DEFAULT_PORT);
            assert!(!settings.enabled);
        }
        assert_eq!(
            McpSettings::from_value(Some(&json!({"port": 65535, "enabled": "yes"}))).port,
            65535
        );
        let on = McpSettings::from_value(Some(&json!({"port": 1, "enabled": true})));
        assert!(on.enabled);
        assert_eq!(on.port, 1);
    }

    #[test]
    fn review_uses_mcp_spellings_and_a_missing_expiry_means_never() {
        assert_eq!(ReviewPolicy::EveryCall.key(), "every-call");
        assert_eq!(ReviewPolicy::FirstSession.key(), "first-session");
        assert_eq!(ReviewPolicy::Never.key(), "never");
        assert_eq!(
            ReviewPolicy::from_key("every-request"),
            None,
            "the assistant spelling is not this setting"
        );
        assert_eq!(ReviewPolicy::from_key("first-only"), None);
        let connection = conn(1).to_string();
        for review in ["every-request", "first-only", "", "every_call"] {
            let settings = McpSettings::from_value(Some(&json!({
                "tokens": [{
                    "id": ID_A,
                    "connection_id": connection,
                    "review": review
                }]
            })));
            assert_eq!(settings.tokens[0].review, ReviewPolicy::EveryCall);
        }
        assert_eq!(
            McpSettings::from_value(Some(&json!({
                "tokens": [{
                    "id": ID_A,
                    "connection_id": connection,
                    "review": "never"
                }]
            })))
            .tokens[0]
                .review,
            ReviewPolicy::Never
        );
        for expiry in [json!(null), json!("soon"), json!(true)] {
            let settings = McpSettings::from_value(Some(&json!({
                "tokens": [{
                    "id": ID_A,
                    "connection_id": connection,
                    "expires_unix_ms": expiry
                }]
            })));
            assert_eq!(settings.tokens[0].expires_unix_ms, None);
        }
        let finite = McpSettings::from_value(Some(&json!({
            "tokens": [{
                "id": ID_A,
                "connection_id": connection,
                "expires_unix_ms": 50,
                "suspended": ""
            }]
        })));
        assert_eq!(finite.tokens[0].expires_unix_ms, Some(50));
        assert_eq!(finite.tokens[0].suspended.as_deref(), Some(""));
    }

    #[test]
    fn environment_label_is_the_stored_key_or_empty() {
        assert_eq!(environment_label(None), "");
        assert_eq!(
            environment_label(Some(Environment::Development)),
            "development"
        );
        assert_eq!(environment_label(Some(Environment::Staging)), "staging");
        assert_eq!(
            environment_label(Some(Environment::Production)),
            "production"
        );
        assert_ne!(environment_label(None), "none");
        assert_ne!(
            environment_label(Some(Environment::Production)),
            "Production"
        );
    }

    #[tokio::test]
    async fn create_stores_the_secret_part_only_after_the_vault_accepts_it() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        let token = mcp_token(ID_A, "ab");
        let secret = token.expose_secret().to_owned();
        let rendered = format!("{:?}", draft(mcp_token(ID_A, "ab")));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains("jykmcp_"));
        registry.create(draft(token)).await.unwrap();
        assert_eq!(vault.sets(), 1);
        assert_eq!(writer.writes(), 1);
        assert_eq!(vault.secret_of(ID_A).as_deref(), Some(secret.as_str()));
        assert!(!vault.secret_of(ID_A).unwrap().contains("jykmcp_"));
        let saved = writer.saved().unwrap();
        assert_eq!(&saved, registry.settings());
        let text = saved.to_value().to_string();
        assert!(!text.contains(&secret));
        assert!(!text.contains("jykmcp_"));
        let record = registry.record(&McpTokenId::parse(ID_A).unwrap()).unwrap();
        assert_eq!(record.name, "Cursor on laptop");
        assert_eq!(record.connection_id, conn(1));
        assert_eq!(record.connection_revision, 4);
        assert_eq!(record.environment_label, "production");
        assert_eq!(record.created_unix_ms, 1_700_000_000_000);
        assert!(record.tools.search_objects);
        assert!(record.tools.describe_relation);
        assert!(!record.tools.run_query);
        assert_eq!(record.review, ReviewPolicy::EveryCall);
        assert_eq!(record.suspended, None);
        assert_eq!(saved.to_value()["tokens"][0]["review"], json!("every-call"));
    }

    #[tokio::test]
    async fn create_saves_nothing_when_the_vault_refuses() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        vault.fail_set(true);
        let token = mcp_token(ID_A, "ab");
        let secret = token.expose_secret().to_owned();
        let error = registry.create(draft(token)).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Authentication);
        assert_eq!(error.message, VAULT_SENTENCE);
        assert_no_secret(&error, &secret, ID_A);
        assert_eq!(writer.writes(), 0);
        assert!(writer.saved().is_none());
        assert!(registry.settings().tokens.is_empty());
        assert!(vault.secret_of(ID_A).is_none());
        assert!(registry.pending_vault_deletions().is_empty());
        assert_eq!(vault.deletes(), 0);
    }

    #[tokio::test]
    async fn create_removes_the_vault_entry_when_saving_the_record_fails() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        writer.fail(true);
        let token = mcp_token(ID_A, "cd");
        let secret = token.expose_secret().to_owned();
        let error = registry.create(draft(token)).await.unwrap_err();
        assert_eq!(error.message, SAVE_SENTENCE);
        assert_no_secret(&error, &secret, ID_A);
        assert_eq!(vault.deletes(), 1);
        assert!(vault.secret_of(ID_A).is_none());
        assert!(registry.pending_vault_deletions().is_empty());
        assert!(registry.settings().tokens.is_empty());
        writer.fail(false);
        registry.set_enabled(true).await.unwrap();
        let text = writer.saved().unwrap().to_value().to_string();
        assert!(!text.contains(&secret));
        assert!(!text.contains(ID_A));
        assert!(writer.saved().unwrap().tokens.is_empty());
    }

    #[tokio::test]
    async fn create_reports_the_save_failure_when_removing_the_vault_entry_fails() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        writer.fail(true);
        vault.fail_delete(true);
        let token = mcp_token(ID_A, "ef");
        let secret = token.expose_secret().to_owned();
        let error = registry.create(draft(token)).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Database);
        assert_eq!(error.message, SAVE_SENTENCE);
        assert_no_secret(&error, &secret, ID_A);
        assert_eq!(vault.secret_of(ID_A).as_deref(), Some(secret.as_str()));
        assert_eq!(
            registry.pending_vault_deletions(),
            &[McpTokenId::parse(ID_A).unwrap()]
        );
        assert!(registry.settings().tokens.is_empty());
        let text = registry.settings().to_value().to_string();
        assert!(!text.contains(&secret));
        assert!(!text.contains(ID_A));
    }

    #[tokio::test]
    async fn create_checks_the_name_the_environment_and_a_duplicate_id() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        let reject = |error: DriverError| {
            assert_eq!(error.code, ErrorCode::Validation);
            assert!(!error.message.contains(ID_A));
        };
        for (name, environment) in [
            ("", "production"),
            ("   ", "production"),
            (&"é".repeat(61), "production"),
            ("Cursor", "Production"),
            ("Cursor", "none"),
            ("Cursor", "PRODUCTION"),
        ] {
            let mut draft = draft(mcp_token(ID_A, "ab"));
            draft.name = name.to_owned();
            draft.environment_label = environment.to_owned();
            reject(registry.create(draft).await.unwrap_err());
        }
        assert_eq!(vault.sets(), 0);
        assert_eq!(writer.writes(), 0);

        let mut long = draft_named(mcp_token(ID_A, "ab"), &"é".repeat(60));
        long.environment_label.clear();
        registry.create(long).await.unwrap();
        assert_eq!(
            registry
                .record(&McpTokenId::parse(ID_A).unwrap())
                .unwrap()
                .name
                .chars()
                .count(),
            60
        );
        assert_eq!(vault.sets(), 1);

        let mut again = draft_named(mcp_token(ID_B, "cd"), &format!("  {}  ", "é".repeat(60)));
        again.environment_label = "development".to_owned();
        let error = registry.create(again).await.unwrap_err();
        assert_eq!(error.message, "A token already has this name.");
        assert_eq!(vault.sets(), 1);

        registry
            .create(draft_named(mcp_token(ID_B, "cd"), "other"))
            .await
            .unwrap();
        let error = registry
            .create(draft_named(mcp_token(ID_B, "ef"), "third"))
            .await
            .unwrap_err();
        assert_eq!(error.message, "This token is already in the list.");
        assert_eq!(registry.settings().tokens.len(), 2);
        assert!(vault.secret_of(ID_B).unwrap().starts_with("cd"));
    }

    #[tokio::test]
    async fn rotate_keeps_the_binding_and_the_old_token_stops_if_its_vault_entry_remains() {
        let (mut registry, vault, _) = harness(McpSettings::default());
        let old = mcp_token(ID_A, "ab");
        let old_secret = old.expose_secret().to_owned();
        let mut created = draft(old);
        created.tools.run_query = true;
        created.review = ReviewPolicy::Never;
        registry.create(created).await.unwrap();
        registry
            .note_profile(conn(1), 9, "production")
            .await
            .unwrap();
        let before = registry
            .record(&McpTokenId::parse(ID_A).unwrap())
            .unwrap()
            .clone();
        vault.fail_delete(true);
        let new = mcp_token(ID_B, "cd");
        let new_secret = new.expose_secret().to_owned();
        let cleanup = registry
            .rotate(&McpTokenId::parse(ID_A).unwrap(), new)
            .await
            .unwrap();
        assert_eq!(cleanup.remaining, vec![McpTokenId::parse(ID_A).unwrap()]);
        assert_eq!(
            registry.pending_vault_deletions(),
            cleanup.remaining.as_slice()
        );
        let record = registry.record(&McpTokenId::parse(ID_B).unwrap()).unwrap();
        assert_eq!(record.name, before.name);
        assert_eq!(record.connection_id, before.connection_id);
        assert_eq!(record.connection_revision, before.connection_revision);
        assert_eq!(record.environment_label, before.environment_label);
        assert_eq!(record.created_unix_ms, before.created_unix_ms);
        assert_eq!(record.expires_unix_ms, before.expires_unix_ms);
        assert!(record.tools.run_query);
        assert_eq!(record.review, ReviewPolicy::Never);
        assert_eq!(record.suspended.as_deref(), Some(CONNECTION_CHANGED));
        assert!(registry.record(&McpTokenId::parse(ID_A).unwrap()).is_none());
        assert_eq!(vault.secret_of(ID_A).as_deref(), Some(old_secret.as_str()));
        assert_eq!(vault.secret_of(ID_B).as_deref(), Some(new_secret.as_str()));
        let gets = vault.gets();
        let error = registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &old_secret, 0)
            .await
            .unwrap_err();
        assert_eq!(error, TokenRefusal::Unknown);
        assert_eq!(vault.gets(), gets);
        registry
            .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &new_secret, 0)
            .await
            .unwrap_err();
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &new_secret, 0)
                .await,
            Err(TokenRefusal::Suspended)
        );
    }

    #[tokio::test]
    async fn rotate_keeps_the_old_record_when_the_new_vault_write_fails() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        let old = mcp_token(ID_A, "ab");
        let old_secret = old.expose_secret().to_owned();
        registry.create(draft(old)).await.unwrap();
        let writes = writer.writes();
        vault.fail_set(true);
        let new = mcp_token(ID_B, "cd");
        let secret = new.expose_secret().to_owned();
        let error = registry
            .rotate(&McpTokenId::parse(ID_A).unwrap(), new)
            .await
            .unwrap_err();
        assert_eq!(error.message, VAULT_SENTENCE);
        assert_no_secret(&error, &secret, ID_B);
        assert_eq!(writer.writes(), writes);
        assert_eq!(
            registry
                .record(&McpTokenId::parse(ID_A).unwrap())
                .unwrap()
                .id
                .as_str(),
            ID_A
        );
        assert!(registry.record(&McpTokenId::parse(ID_B).unwrap()).is_none());
        assert!(vault.secret_of(ID_B).is_none());
        assert_eq!(vault.deletes(), 0);
        registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &old_secret, 0)
            .await
            .unwrap();
        let error = registry
            .rotate(&McpTokenId::parse(ID_C).unwrap(), mcp_token(ID_B, "cd"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(!error.message.contains(ID_C));
    }

    #[tokio::test]
    async fn rotate_removes_the_new_vault_entry_when_the_record_cannot_be_saved() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        registry.create(draft(mcp_token(ID_A, "ab"))).await.unwrap();
        writer.fail(true);
        let new = mcp_token(ID_B, "cd");
        let secret = new.expose_secret().to_owned();
        let error = registry
            .rotate(&McpTokenId::parse(ID_A).unwrap(), new)
            .await
            .unwrap_err();
        assert_eq!(error.message, SAVE_SENTENCE);
        assert_no_secret(&error, &secret, ID_B);
        assert!(vault.secret_of(ID_B).is_none());
        assert!(registry.pending_vault_deletions().is_empty());
        assert_eq!(
            registry
                .record(&McpTokenId::parse(ID_A).unwrap())
                .unwrap()
                .id
                .as_str(),
            ID_A
        );

        let (mut registry, vault, writer) = harness(McpSettings::default());
        registry.create(draft(mcp_token(ID_A, "ab"))).await.unwrap();
        writer.fail(true);
        vault.fail_delete(true);
        let error = registry
            .rotate(&McpTokenId::parse(ID_A).unwrap(), mcp_token(ID_B, "ef"))
            .await
            .unwrap_err();
        assert_eq!(error.message, SAVE_SENTENCE);
        assert_eq!(vault.secret_of(ID_B).unwrap().len(), 64);
        assert_eq!(
            registry.pending_vault_deletions(),
            &[McpTokenId::parse(ID_B).unwrap()]
        );
        assert!(registry.record(&McpTokenId::parse(ID_B).unwrap()).is_none());
        let same = mcp_token(ID_A, "11");
        let error = registry
            .rotate(&McpTokenId::parse(ID_A).unwrap(), same)
            .await
            .unwrap_err();
        assert_eq!(error.message, "Rotate needs a new token.");
        assert_eq!(vault.sets(), 2);
    }

    #[tokio::test]
    async fn revoke_drops_the_record_and_the_cached_secret_before_the_vault_delete() {
        let (mut registry, vault, _) = harness(McpSettings::default());
        let token = mcp_token(ID_A, "ab");
        let secret = token.expose_secret().to_owned();
        registry.create(draft(token)).await.unwrap();
        registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
            .await
            .unwrap();
        let error = registry
            .revoke(&McpTokenId::parse(ID_C).unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(!error.message.contains(ID_C));

        vault.fail_delete(true);
        let gets = vault.gets();
        let cleanup = registry
            .revoke(&McpTokenId::parse(ID_A).unwrap())
            .await
            .unwrap();
        assert_eq!(cleanup.remaining, vec![McpTokenId::parse(ID_A).unwrap()]);
        assert!(registry.settings().tokens.is_empty());
        assert_eq!(vault.secret_of(ID_A).as_deref(), Some(secret.as_str()));
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::Unknown)
        );
        assert_eq!(vault.gets(), gets);
        let text = registry.settings().to_value().to_string();
        assert!(!text.contains(&secret));
        assert!(!text.contains("pending"));
    }

    #[tokio::test]
    async fn delete_connection_removes_only_that_connections_tokens() {
        let (mut registry, vault, writer) = harness(McpSettings::default());
        let mut first = draft(mcp_token(ID_A, "ab"));
        first.connection_id = conn(1);
        let mut second = draft_named(mcp_token(ID_B, "cd"), "other laptop");
        second.connection_id = conn(1);
        let mut third = draft_named(mcp_token(ID_C, "ef"), "elsewhere");
        third.connection_id = conn(2);
        registry.create(first).await.unwrap();
        registry.create(second).await.unwrap();
        registry.create(third).await.unwrap();
        let writes = writer.writes();
        let cleanup = registry.delete_connection(conn(9)).await.unwrap();
        assert!(cleanup.remaining.is_empty());
        assert_eq!(writer.writes(), writes);

        vault.fail_delete(true);
        let cleanup = registry.delete_connection(conn(1)).await.unwrap();
        assert_eq!(
            cleanup.remaining,
            vec![
                McpTokenId::parse(ID_A).unwrap(),
                McpTokenId::parse(ID_B).unwrap()
            ]
        );
        assert_eq!(registry.settings().tokens.len(), 1);
        assert_eq!(
            registry.settings().tokens[0].id,
            McpTokenId::parse(ID_C).unwrap()
        );
        assert!(vault.secret_of(ID_A).is_some());
        assert!(vault.secret_of(ID_B).is_some());
        registry
            .secret_matches(&McpTokenId::parse(ID_C).unwrap(), &"ef".repeat(32), 0)
            .await
            .unwrap();
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &"ab".repeat(32), 0)
                .await,
            Err(TokenRefusal::Unknown)
        );
    }

    #[tokio::test]
    async fn note_profile_suspends_on_a_different_revision_or_environment_key() {
        let settings = McpSettings {
            tokens: vec![
                record(ID_A, conn(1), 1, "", None),
                record(ID_B, conn(1), 4, "development", None),
                record(ID_C, conn(1), 4, "staging", None),
                record(
                    "cdefghij23",
                    conn(1),
                    1,
                    "production",
                    Some("later-version"),
                ),
                record(
                    "dddddddddd",
                    conn(1),
                    3,
                    "staging",
                    Some(CONNECTION_CHANGED),
                ),
                record("eeeeeeeeee", conn(2), 1, "", None),
            ],
            ..McpSettings::default()
        };
        let (mut registry, vault, writer) = harness(settings);
        vault.plant(ID_A, &"ab".repeat(32));
        registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &"ab".repeat(32), 0)
            .await
            .unwrap();
        let gets = vault.gets();
        registry.note_profile(conn(1), 4, "staging").await.unwrap();
        assert_eq!(writer.writes(), 1);
        assert_eq!(
            suspended_of(&registry, ID_A).as_deref(),
            Some(CONNECTION_CHANGED)
        );
        assert_eq!(
            suspended_of(&registry, ID_B).as_deref(),
            Some(CONNECTION_CHANGED)
        );
        assert_eq!(suspended_of(&registry, ID_C), None);
        assert_eq!(
            suspended_of(&registry, "cdefghij23").as_deref(),
            Some("later-version")
        );
        assert_eq!(
            suspended_of(&registry, "dddddddddd").as_deref(),
            Some(CONNECTION_CHANGED)
        );
        assert_eq!(suspended_of(&registry, "eeeeeeeeee"), None);
        assert_eq!(
            registry
                .record(&McpTokenId::parse(ID_A).unwrap())
                .unwrap()
                .connection_revision,
            1
        );
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &"ab".repeat(32), 0)
                .await,
            Err(TokenRefusal::Suspended)
        );
        assert_eq!(vault.gets(), gets);
        // The same live profile does not clear a suspension and does not
        // write again: confirming is what stores the new revision.
        registry.note_profile(conn(1), 4, "staging").await.unwrap();
        assert_eq!(writer.writes(), 1);
        assert_eq!(
            suspended_of(&registry, ID_A).as_deref(),
            Some(CONNECTION_CHANGED)
        );
        assert_eq!(suspended_of(&registry, ID_C), None);
    }

    #[tokio::test]
    async fn confirm_clears_only_connection_changed_and_stores_the_live_binding() {
        let settings = McpSettings {
            tokens: vec![
                record(ID_A, conn(1), 1, "", Some(CONNECTION_CHANGED)),
                record(ID_B, conn(1), 2, "production", Some("later-version")),
                record(ID_C, conn(1), 3, "staging", None),
            ],
            ..McpSettings::default()
        };
        let (mut registry, _, writer) = harness(settings);
        registry
            .confirm(&McpTokenId::parse(ID_A).unwrap(), 8, "development")
            .await
            .unwrap();
        let cleared = registry.record(&McpTokenId::parse(ID_A).unwrap()).unwrap();
        assert_eq!(cleared.suspended, None);
        assert_eq!(cleared.connection_revision, 8);
        assert_eq!(cleared.environment_label, "development");

        let error = registry
            .confirm(&McpTokenId::parse(ID_B).unwrap(), 9, "staging")
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "This token stays suspended. This version cannot clear that reason."
        );
        assert!(!error.message.contains("later-version"));
        assert!(!error.message.contains(ID_B));
        let kept = registry.record(&McpTokenId::parse(ID_B).unwrap()).unwrap();
        assert_eq!(kept.suspended.as_deref(), Some("later-version"));
        assert_eq!(kept.connection_revision, 2);

        let writes = writer.writes();
        let error = registry
            .confirm(&McpTokenId::parse(ID_C).unwrap(), 3, "none")
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(writer.writes(), writes);
        assert_eq!(
            registry
                .record(&McpTokenId::parse(ID_C).unwrap())
                .unwrap()
                .environment_label,
            "staging"
        );
        let error = registry
            .confirm(&McpTokenId::parse(ID_C).unwrap(), 3, "Production")
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        registry
            .confirm(&McpTokenId::parse(ID_C).unwrap(), 11, "")
            .await
            .unwrap();
        let updated = registry.record(&McpTokenId::parse(ID_C).unwrap()).unwrap();
        assert_eq!(updated.connection_revision, 11);
        assert_eq!(updated.environment_label, "");
        assert_eq!(updated.suspended, None);
    }

    #[tokio::test]
    async fn set_expiry_moves_later_or_to_never_only_when_confirmed() {
        let (mut registry, _, writer) = harness(McpSettings {
            tokens: vec![record(ID_A, conn(1), 1, "", None)],
            ..McpSettings::default()
        });
        let id = McpTokenId::parse(ID_A).unwrap();
        registry.set_expiry(&id, Some(100), false).await.unwrap();
        let writes = writer.writes();
        registry.set_expiry(&id, Some(100), false).await.unwrap();
        assert_eq!(writer.writes(), writes);
        registry.set_expiry(&id, Some(90), false).await.unwrap();
        let error = registry
            .set_expiry(&id, Some(110), false)
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "Choose an earlier expiry, or confirm this one."
        );
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, Some(90));
        let error = registry.set_expiry(&id, None, false).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, Some(90));
        registry.set_expiry(&id, Some(110), true).await.unwrap();
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, Some(110));
        registry.set_expiry(&id, None, true).await.unwrap();
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, None);
        registry.set_expiry(&id, Some(50), false).await.unwrap();
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, Some(50));
        registry.set_expiry(&id, None, true).await.unwrap();
        registry.set_expiry(&id, None, false).await.unwrap();
        assert_eq!(registry.record(&id).unwrap().expires_unix_ms, None);
    }

    #[tokio::test]
    async fn set_name_tools_review_port_and_enabled_save() {
        let (mut registry, _, writer) = harness(McpSettings {
            tokens: vec![record(
                ID_A,
                conn(1),
                1,
                "production",
                Some("later-version"),
            )],
            ..McpSettings::default()
        });
        let id = McpTokenId::parse(ID_A).unwrap();
        registry.set_name(&id, "  Beta  ").await.unwrap();
        assert_eq!(registry.record(&id).unwrap().name, "Beta");
        assert_eq!(
            registry.record(&id).unwrap().suspended.as_deref(),
            Some("later-version")
        );
        let writes = writer.writes();
        registry.set_name(&id, " Beta ").await.unwrap();
        assert_eq!(writer.writes(), writes);
        let error = registry.set_name(&id, "   ").await.unwrap_err();
        assert_eq!(error.message, "Enter a name for the token.");
        let error = registry.set_name(&id, &"a".repeat(61)).await.unwrap_err();
        assert_eq!(error.message, "Enter a name of at most 60 characters.");
        registry
            .create(draft_named(mcp_token(ID_B, "ab"), "Gamma"))
            .await
            .unwrap();
        let error = registry.set_name(&id, "Gamma").await.unwrap_err();
        assert_eq!(error.message, "A token already has this name.");

        let tools = EnabledTools {
            search_objects: false,
            describe_relation: true,
            run_query: true,
        };
        registry.set_tools(&id, tools).await.unwrap();
        registry
            .set_review(&id, ReviewPolicy::FirstSession)
            .await
            .unwrap();
        assert_eq!(registry.record(&id).unwrap().tools, tools);
        assert_eq!(
            registry.record(&id).unwrap().review,
            ReviewPolicy::FirstSession
        );
        assert_eq!(
            registry.settings().to_value()["tokens"][0]["review"],
            json!("first-session")
        );

        let error = registry.set_port(0).await.unwrap_err();
        assert_eq!(error.message, "Enter a port between 1 and 65535.");
        assert_eq!(registry.settings().port, DEFAULT_PORT);
        registry.set_port(65535).await.unwrap();
        registry.set_port(1).await.unwrap();
        registry.set_enabled(true).await.unwrap();
        assert_eq!(registry.settings().port, 1);
        assert!(registry.settings().enabled);
        let writes = writer.writes();
        registry.set_enabled(true).await.unwrap();
        registry.set_port(1).await.unwrap();
        assert_eq!(writer.writes(), writes);
    }

    #[tokio::test]
    async fn secret_matches_uses_the_cache_and_distinguishes_each_refusal() {
        let settings = McpSettings {
            tokens: vec![
                record(ID_A, conn(1), 1, "", None),
                record(ID_B, conn(1), 1, "", Some(CONNECTION_CHANGED)),
                TokenRecord {
                    expires_unix_ms: Some(100),
                    suspended: Some(CONNECTION_CHANGED.to_owned()),
                    ..record(ID_C, conn(1), 1, "", None)
                },
            ],
            ..McpSettings::default()
        };
        let (mut registry, vault, _) = harness(settings);
        let secret = "ab".repeat(32);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse("eeeeeeeeee").unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::Unknown)
        );
        assert_eq!(vault.gets(), 0);

        vault.plant(ID_A, &secret);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 999)
                .await,
            Ok(())
        );
        assert_eq!(vault.gets(), 1);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 999)
                .await,
            Ok(())
        );
        assert_eq!(vault.gets(), 1);
        let mut flipped = secret.clone();
        flipped.pop();
        flipped.push('c');
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &flipped, 999)
                .await,
            Err(TokenRefusal::Mismatch)
        );
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), "", 999)
                .await,
            Err(TokenRefusal::Mismatch)
        );
        assert_eq!(vault.gets(), 1);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 1_000)
                .await,
            Err(TokenRefusal::Expired)
        );
        registry
            .set_expiry(&McpTokenId::parse(ID_A).unwrap(), Some(5_000), true)
            .await
            .unwrap();
        registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 1_001)
            .await
            .unwrap();
        assert_eq!(vault.gets(), 2);

        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &flipped, 0)
                .await,
            Err(TokenRefusal::Suspended)
        );
        assert_eq!(vault.gets(), 2);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_C).unwrap(), &secret, 100)
                .await,
            Err(TokenRefusal::Expired)
        );

        let (mut registry, vault, _) = harness(McpSettings {
            tokens: vec![record(ID_A, conn(1), 1, "", None)],
            ..McpSettings::default()
        });
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::MissingFromVault)
        );
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::MissingFromVault)
        );
        assert_eq!(vault.gets(), 2);
        vault.plant(ID_A, &"cd".repeat(32));
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::Mismatch)
        );
        let gets = vault.gets();
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::Mismatch)
        );
        assert_eq!(vault.gets(), gets);
        vault.fail_get(true);
        let (mut registry, vault, _) = harness(McpSettings {
            tokens: vec![record(ID_B, conn(1), 1, "", None)],
            ..McpSettings::default()
        });
        vault.fail_get(true);
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::VaultUnavailable)
        );
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::VaultUnavailable)
        );
        assert_eq!(vault.gets(), 2);
    }

    #[tokio::test]
    async fn a_vault_timeout_is_the_unavailable_vault_error_without_the_secret() {
        let settings = McpSettings {
            tokens: vec![record(ID_A, conn(1), 1, "", None)],
            ..McpSettings::default()
        };
        let (registry, vault, writer) = harness(settings);
        assert_eq!(registry.vault_limit(), VAULT_TIMEOUT);
        assert_eq!(VAULT_TIMEOUT, Duration::from_secs(10));
        let mut registry = registry.with_vault_limit(Duration::from_millis(20));
        vault.hang(true);
        let secret = "ab".repeat(32);
        let started = std::time::Instant::now();
        let error = registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
            .await
            .unwrap_err();
        assert_eq!(error, TokenRefusal::VaultUnavailable);
        let token = mcp_token(ID_B, "cd");
        let created = token.expose_secret().to_owned();
        let error = registry.create(draft(token)).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.code, ErrorCode::Authentication);
        assert_eq!(error.message, VAULT_SENTENCE);
        assert_no_secret(&error, &created, ID_B);
        assert_eq!(writer.writes(), 0);
        assert_eq!(registry.settings().tokens.len(), 1);
        assert!(vault.secret_of(ID_B).is_none());
        assert_eq!(
            registry
                .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
                .await,
            Err(TokenRefusal::VaultUnavailable)
        );
    }

    #[tokio::test]
    async fn retry_vault_deletions_walks_the_ids_kept_in_memory() {
        let (mut registry, vault, _) = harness(McpSettings::default());
        registry.create(draft(mcp_token(ID_A, "ab"))).await.unwrap();
        registry
            .create(draft_named(mcp_token(ID_B, "cd"), "second"))
            .await
            .unwrap();
        let cleanup = registry.retry_vault_deletions().await.unwrap();
        assert!(cleanup.remaining.is_empty());
        vault.fail_delete(true);
        registry
            .revoke(&McpTokenId::parse(ID_A).unwrap())
            .await
            .unwrap();
        registry
            .revoke(&McpTokenId::parse(ID_B).unwrap())
            .await
            .unwrap();
        assert_eq!(registry.pending_vault_deletions().len(), 2);
        let error = registry.retry_vault_deletions().await.unwrap_err();
        assert_eq!(error.message, VAULT_SENTENCE);
        assert!(!error.message.contains(ID_A));
        assert!(!error.message.contains(ID_B));
        assert_eq!(registry.pending_vault_deletions().len(), 2);
        assert!(vault.secret_of(ID_A).is_some());
        vault.fail_delete(false);
        let cleanup = registry.retry_vault_deletions().await.unwrap();
        assert!(cleanup.remaining.is_empty());
        assert!(registry.pending_vault_deletions().is_empty());
        assert!(vault.secret_of(ID_A).is_none());
        assert!(vault.secret_of(ID_B).is_none());
    }

    #[tokio::test]
    async fn an_older_settings_write_cannot_land_after_a_newer_one() {
        let dir = std::env::temp_dir().join(format!("jykre-mcp-{}", ConnectionId::new()));
        let _remove = RemoveDir(dir.clone());
        assert_eq!(load_settings(&dir).await.unwrap(), McpSettings::default());
        let token = record(ID_A, conn(1), 4, "staging", Some("later-version"));
        let newer = McpSettings {
            enabled: true,
            port: 9,
            tokens: vec![token],
        };
        let older = McpSettings {
            enabled: false,
            port: 1,
            tokens: Vec::new(),
        };
        let older_ticket = MCP_WRITES.ask();
        let newer_ticket = MCP_WRITES.ask();
        save_settings(&dir, &newer, newer_ticket).await.unwrap();
        save_settings(&dir, &older, older_ticket).await.unwrap();
        assert_eq!(load_settings(&dir).await.unwrap(), newer);
        let registry = TokenRegistry::open(&dir, FakeVault::default())
            .await
            .unwrap();
        assert_eq!(registry.settings(), &newer);
        let text = registry.settings().to_value().to_string();
        assert!(!text.contains("jykmcp_"));
        assert_eq!(
            registry.settings().tokens[0].suspended.as_deref(),
            Some("later-version")
        );
    }

    #[tokio::test]
    async fn debug_of_the_registry_omits_the_cached_secret() {
        let (mut registry, vault, _) = harness(McpSettings::default());
        let token = mcp_token(ID_A, "ab");
        let secret = token.expose_secret().to_owned();
        registry.create(draft(token)).await.unwrap();
        registry
            .secret_matches(&McpTokenId::parse(ID_A).unwrap(), &secret, 0)
            .await
            .unwrap();
        vault.fail_delete(true);
        registry
            .revoke(&McpTokenId::parse(ID_A).unwrap())
            .await
            .unwrap();
        registry
            .create(draft_named(mcp_token(ID_B, "cd"), "kept"))
            .await
            .unwrap();
        let kept = mcp_token(ID_B, "cd").expose_secret().to_owned();
        registry
            .secret_matches(&McpTokenId::parse(ID_B).unwrap(), &kept, 0)
            .await
            .unwrap();
        let rendered = format!("{registry:?}");
        assert!(rendered.contains("cached_secrets"));
        assert!(rendered.contains(ID_A));
        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains(&kept));
        assert!(!rendered.contains("Secret("));
        assert!(!rendered.contains("jykmcp_"));
    }
}
