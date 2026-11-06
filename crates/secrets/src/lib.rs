//! Native credential vault. There is deliberately no plaintext file fallback.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::{
    ConnectionId, Credentials, DriverError, DriverResult, ErrorCode, SecretString, async_trait,
};
use secrecy::ExposeSecret;

const SERVICE: &str = "JykreStudio";

fn unavailable() -> DriverError {
    DriverError::new(
        ErrorCode::Authentication,
        "System vault unavailable. The password will be used only in this session.",
    )
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, id: ConnectionId) -> DriverResult<Credentials>;
    async fn set(&self, id: ConnectionId, password: SecretString) -> DriverResult<()>;
    async fn delete(&self, id: ConnectionId) -> DriverResult<()>;
}

#[derive(Default)]
pub struct SystemSecretStore;
fn entry(id: ConnectionId) -> DriverResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("connection:{id}:password")).map_err(|_| unavailable())
}

#[async_trait]
impl SecretStore for SystemSecretStore {
    async fn get(&self, id: ConnectionId) -> DriverResult<Credentials> {
        tokio::task::spawn_blocking(move || match entry(id)?.get_password() {
            Ok(password) => Ok(Credentials {
                password: Some(SecretString::from(password)),
            }),
            Err(keyring::Error::NoEntry) => Ok(Credentials::default()),
            Err(_) => Err(unavailable()),
        })
        .await
        .map_err(|_| unavailable())?
    }
    async fn set(&self, id: ConnectionId, password: SecretString) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || {
            entry(id)?
                .set_password(password.expose_secret())
                .map_err(|_| unavailable())
        })
        .await
        .map_err(|_| unavailable())?
    }
    async fn delete(&self, id: ConnectionId) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || match entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(unavailable()),
        })
        .await
        .map_err(|_| unavailable())?
    }
}

fn key_unavailable() -> DriverError {
    DriverError::new(
        ErrorCode::Authentication,
        "System vault unavailable. The API key could not be read, saved or removed.",
    )
}

/// The vault account that holds the API key of one assistant provider.
///
/// Only the fixed accounts built by the constructors exist, so a name typed
/// in the UI can never reach the vault. The account name is not a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ApiKeyAccount(&'static str);

impl ApiKeyAccount {
    /// The Anthropic API key.
    pub const fn anthropic() -> Self {
        Self("assistant:anthropic:api-key")
    }

    /// The OpenAI API key.
    pub const fn openai() -> Self {
        Self("assistant:openai:api-key")
    }

    /// The API key of an OpenAI-compatible endpoint.
    pub const fn openai_compatible() -> Self {
        Self("assistant:openai-compatible:api-key")
    }

    /// The account name under service `JykreStudio`.
    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// API keys of the assistant's providers, kept only in the OS vault.
#[async_trait]
pub trait ApiKeyStore: Send + Sync {
    /// The stored key, or `None` when the account has no entry.
    async fn get(&self, account: ApiKeyAccount) -> DriverResult<Option<SecretString>>;
    async fn set(&self, account: ApiKeyAccount, key: SecretString) -> DriverResult<()>;
    /// Removes the key; an account without an entry is not an error.
    async fn delete(&self, account: ApiKeyAccount) -> DriverResult<()>;
}

fn api_key_entry(account: ApiKeyAccount) -> DriverResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, account.name()).map_err(|_| key_unavailable())
}

/// Maps a vault read: no entry is `None`; any other failure is reported
/// without its detail, which may come from the platform.
fn read_api_key(read: keyring::Result<String>) -> DriverResult<Option<SecretString>> {
    match read {
        Ok(key) => Ok(Some(SecretString::from(key))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(key_unavailable()),
    }
}

/// Maps a vault removal: an entry already gone counts as removed.
fn removed_api_key(removal: keyring::Result<()>) -> DriverResult<()> {
    match removal {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(key_unavailable()),
    }
}

#[async_trait]
impl ApiKeyStore for SystemSecretStore {
    async fn get(&self, account: ApiKeyAccount) -> DriverResult<Option<SecretString>> {
        tokio::task::spawn_blocking(move || read_api_key(api_key_entry(account)?.get_password()))
            .await
            .map_err(|_| key_unavailable())?
    }
    async fn set(&self, account: ApiKeyAccount, key: SecretString) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || {
            api_key_entry(account)?
                .set_password(key.expose_secret())
                .map_err(|_| key_unavailable())
        })
        .await
        .map_err(|_| key_unavailable())?
    }
    async fn delete(&self, account: ApiKeyAccount) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || {
            removed_api_key(api_key_entry(account)?.delete_credential())
        })
        .await
        .map_err(|_| key_unavailable())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_KEY: &str = "sk-jykre-synthetic-unit-fixture";

    fn platform_failure_mentioning_the_key() -> keyring::Error {
        keyring::Error::PlatformFailure(SYNTHETIC_KEY.into())
    }

    #[test]
    fn api_key_accounts_are_the_fixed_assistant_names() {
        assert_eq!(
            ApiKeyAccount::anthropic().name(),
            "assistant:anthropic:api-key"
        );
        assert_eq!(ApiKeyAccount::openai().name(), "assistant:openai:api-key");
        assert_eq!(
            ApiKeyAccount::openai_compatible().name(),
            "assistant:openai-compatible:api-key"
        );
    }

    #[test]
    fn api_key_accounts_never_collide_with_connection_passwords() {
        let accounts = [
            ApiKeyAccount::anthropic(),
            ApiKeyAccount::openai(),
            ApiKeyAccount::openai_compatible(),
        ];
        for (index, account) in accounts.iter().enumerate() {
            assert!(!account.name().starts_with("connection:"));
            assert!(accounts[index + 1..].iter().all(|other| other != account));
        }
    }

    #[test]
    fn a_missing_api_key_reads_as_none() {
        assert!(
            read_api_key(Err(keyring::Error::NoEntry))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_stored_api_key_reads_back_as_a_secret() {
        let key = read_api_key(Ok(SYNTHETIC_KEY.to_owned())).unwrap().unwrap();
        assert_eq!(key.expose_secret(), SYNTHETIC_KEY);
        assert!(!format!("{key:?}").contains(SYNTHETIC_KEY));
    }

    #[test]
    fn removing_a_missing_api_key_succeeds() {
        removed_api_key(Ok(())).unwrap();
        removed_api_key(Err(keyring::Error::NoEntry)).unwrap();
    }

    #[test]
    fn vault_failures_are_reported_without_their_detail() {
        let errors = [
            read_api_key(Err(platform_failure_mentioning_the_key())).unwrap_err(),
            removed_api_key(Err(platform_failure_mentioning_the_key())).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.code, ErrorCode::Authentication);
            assert!(error.message.contains("API key"));
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(SYNTHETIC_KEY), "{shown}");
        }
    }
}
