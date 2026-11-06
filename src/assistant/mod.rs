//! The AI assistant's application side (ADR-0028): its preferences, the
//! API key in the system vault, what may be told about a tab
//! ([`facts`]), the tools over the metadata cache ([`tools`]), the texts
//! the model is given ([`prompts`]), the agentic loop ([`run`]) and putting
//! a suggestion into the editor ([`apply`]). The provider side is the
//! `jykre-assistant` crate. No view decides anything here, and nothing here
//! reads a result's values or opens a session.
pub mod apply;
pub mod facts;
pub mod prompts;
pub mod run;
pub mod tools;

use jykre_assistant::{Price, ProviderConfig, ProviderKind};
use jykre_core::{DriverResult, SecretString};
use jykre_secrets::{ApiKeyAccount, ApiKeyStore, SystemSecretStore};
use run::ReviewMode;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// The setting the preferences are kept under in `config.db`.
pub const PREFERENCES_KEY: &str = "assistant.preferences";

/// What the user chose in **Preferences › Assistant**. The API key is not
/// here: it lives in the system vault only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantPreferences {
    /// `None` turns the assistant off.
    pub provider: Option<ProviderKind>,
    pub model: String,
    pub endpoint: Option<String>,
    pub review: ReviewMode,
    /// Prices per model name, entered by the user.
    pub prices: BTreeMap<String, Price>,
}

impl AssistantPreferences {
    /// Reads what [`Self::to_value`] wrote; anything missing or unknown
    /// takes its default.
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        let prices = value
            .get("prices")
            .and_then(Value::as_object)
            .map(|prices| {
                prices
                    .iter()
                    .filter_map(|(model, price)| {
                        serde_json::from_value::<Price>(price.clone())
                            .ok()
                            .filter(|price| {
                                price.input_per_mtok >= 0.0 && price.output_per_mtok >= 0.0
                            })
                            .map(|price| (model.clone(), price))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            provider: text("provider").and_then(ProviderKind::from_key),
            model: text("model").unwrap_or_default().to_owned(),
            endpoint: text("endpoint")
                .filter(|endpoint| !endpoint.trim().is_empty())
                .map(str::to_owned),
            review: text("review")
                .and_then(ReviewMode::from_key)
                .unwrap_or_default(),
            prices,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "provider": self.provider.map(ProviderKind::key),
            "model": self.model,
            "endpoint": self.endpoint,
            "review": self.review.key(),
            "prices": self.prices,
        })
    }

    /// The provider to call, when the assistant is on and complete.
    pub fn config(&self) -> Option<ProviderConfig> {
        let kind = self.provider?;
        (!self.model.trim().is_empty()).then(|| ProviderConfig {
            kind,
            model: self.model.trim().to_owned(),
            endpoint: self.endpoint.clone(),
        })
    }

    /// The price of the configured model, if the user entered one.
    pub fn price(&self) -> Option<Price> {
        self.prices.get(self.model.trim()).copied()
    }
}

/// The writes of the preferences: the last one asked for wins.
pub static PREFERENCE_WRITES: crate::connection_store::LatestWrite =
    crate::connection_store::LatestWrite::new();

/// Reads the preferences kept in the data folder `dir`.
pub async fn load_preferences(dir: &Path) -> DriverResult<AssistantPreferences> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let value = repository.setting(PREFERENCES_KEY).await;
    repository.close().await;
    Ok(AssistantPreferences::from_value(value?.as_ref()))
}

/// Saves the preferences for the write `ticket` of [`PREFERENCE_WRITES`].
pub async fn save_preferences(
    dir: &Path,
    preferences: AssistantPreferences,
    ticket: u64,
) -> DriverResult<()> {
    PREFERENCE_WRITES
        .run(ticket, async {
            let repository = jykre_storage::ConfigRepository::open(dir).await?;
            let result = repository
                .set_setting(PREFERENCES_KEY, &preferences.to_value())
                .await;
            repository.close().await;
            result
        })
        .await
        .unwrap_or(Ok(()))
}

/// The vault account of a provider's key; a provider without keys has none.
pub fn key_account(kind: ProviderKind) -> Option<ApiKeyAccount> {
    match kind {
        ProviderKind::Anthropic => Some(ApiKeyAccount::anthropic()),
        ProviderKind::OpenAi => Some(ApiKeyAccount::openai()),
        ProviderKind::OpenAiCompatible => Some(ApiKeyAccount::openai_compatible()),
        ProviderKind::Ollama => None,
    }
}

/// The provider's key from the vault, within the vault's time limit.
pub async fn read_key(kind: ProviderKind) -> DriverResult<Option<SecretString>> {
    let Some(account) = key_account(kind) else {
        return Ok(None);
    };
    crate::connection_store::vault_within(
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.get(account),
    )
    .await
}

/// Saves the provider's key in the vault.
pub async fn save_key(kind: ProviderKind, key: SecretString) -> DriverResult<()> {
    let Some(account) = key_account(kind) else {
        return Ok(());
    };
    crate::connection_store::vault_within(
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.set(account, key),
    )
    .await
}

/// Removes the provider's key from the vault.
pub async fn remove_key(kind: ProviderKind) -> DriverResult<()> {
    let Some(account) = key_account(kind) else {
        return Ok(());
    };
    crate::connection_store::vault_within(
        crate::connection_store::VAULT_TIMEOUT,
        SystemSecretStore.delete(account),
    )
    .await
}

#[cfg(test)]
mod tests;
