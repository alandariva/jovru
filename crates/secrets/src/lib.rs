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

/// The vault account that holds the API key of one assistant provider, or
/// the headers of the OpenTelemetry collector.
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

    /// The headers sent to the OpenTelemetry collector (ADR-0029), usually
    /// its authentication.
    pub const fn telemetry_headers() -> Self {
        Self("telemetry:otlp:headers")
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

// Fixed sentence: the UI shows it, and the platform error may repeat the
// token or the account id.
fn token_unavailable() -> DriverError {
    DriverError::new(
        ErrorCode::Authentication,
        "System vault unavailable. The token could not be read, saved or removed.",
    )
}

/// Vault account `mcp:token:<id>`. `<id>` is exactly 10 lowercase base32
/// characters. The id is not a secret; the value stored under the account is.
///
/// The id is stored inline so the account is [`Copy`] and can move into the
/// vault's blocking task. [`Self::parse`] is the only constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct McpTokenAccount {
    id: [u8; 10],
}

impl McpTokenAccount {
    /// `None` when `id` is not exactly 10 characters from
    /// `abcdefghijklmnopqrstuvwxyz234567`.
    pub fn parse(id: &str) -> Option<Self> {
        let bytes: [u8; 10] = id.as_bytes().try_into().ok()?;
        if !bytes.iter().copied().all(Self::is_lowercase_base32) {
            return None;
        }
        Some(Self { id: bytes })
    }

    /// `mcp:token:<id>`.
    pub fn name(self) -> String {
        let id = self.id();
        format!("mcp:token:{id}")
    }

    /// The validated `<id>`. Borrows the stored bytes, so this does not allocate.
    pub fn id(&self) -> &str {
        // `parse` only stores the lowercase base32 alphabet, which is ASCII,
        // so these bytes are UTF-8. The empty string keeps this path free of
        // panics; a value this type can hold never takes it.
        std::str::from_utf8(&self.id).unwrap_or("")
    }

    // RFC 4648 base32, lowercase and without padding. Excludes `0`, `1`, `8`,
    // `9`, `=` and every other byte.
    const fn is_lowercase_base32(byte: u8) -> bool {
        matches!(byte, b'a'..=b'z' | b'2'..=b'7')
    }
}

/// Token secrets kept only in the OS vault, under service `JykreStudio`.
///
/// `get` returns `Ok(None)` when the account has no entry. `delete` of a
/// missing entry is `Ok(())`. A failure is a fixed message and does not
/// include the token, the account id or the platform's detail. This trait
/// does not apply a time limit; the caller does, as it does for [`ApiKeyStore`].
#[async_trait]
pub trait McpTokenStore: Send + Sync {
    /// The stored token, or `None` when the account has no entry.
    async fn get(&self, account: McpTokenAccount) -> DriverResult<Option<SecretString>>;
    /// Saves the token under `account`.
    async fn set(&self, account: McpTokenAccount, token: SecretString) -> DriverResult<()>;
    /// Removes the token; an account without an entry is not an error.
    async fn delete(&self, account: McpTokenAccount) -> DriverResult<()>;
}

fn mcp_token_entry(account: McpTokenAccount) -> DriverResult<keyring::Entry> {
    keyring::Entry::new(SERVICE, &account.name()).map_err(|_| token_unavailable())
}

/// Maps a vault read: no entry is `None`; any other failure is reported
/// without its detail, which may come from the platform.
fn read_mcp_token(read: keyring::Result<String>) -> DriverResult<Option<SecretString>> {
    match read {
        Ok(token) => Ok(Some(SecretString::from(token))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(token_unavailable()),
    }
}

/// Maps a vault removal: an entry already gone counts as removed.
fn removed_mcp_token(removal: keyring::Result<()>) -> DriverResult<()> {
    match removal {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(token_unavailable()),
    }
}

#[async_trait]
impl McpTokenStore for SystemSecretStore {
    async fn get(&self, account: McpTokenAccount) -> DriverResult<Option<SecretString>> {
        tokio::task::spawn_blocking(move || {
            read_mcp_token(mcp_token_entry(account)?.get_password())
        })
        .await
        .map_err(|_| token_unavailable())?
    }
    async fn set(&self, account: McpTokenAccount, token: SecretString) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || {
            mcp_token_entry(account)?
                .set_password(token.expose_secret())
                .map_err(|_| token_unavailable())
        })
        .await
        .map_err(|_| token_unavailable())?
    }
    async fn delete(&self, account: McpTokenAccount) -> DriverResult<()> {
        tokio::task::spawn_blocking(move || {
            removed_mcp_token(mcp_token_entry(account)?.delete_credential())
        })
        .await
        .map_err(|_| token_unavailable())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTHETIC_KEY: &str = "sk-jykre-synthetic-unit-fixture";
    const SYNTHETIC_TOKEN: &str = "jykre-synthetic-mcp-unit-fixture";
    // Lowercase RFC 4648 base32, without padding.
    const BASE32: &str = "abcdefghijklmnopqrstuvwxyz234567";

    // Both impls apply if `McpTokenAccount: From<String>`, so `_` cannot be
    // inferred and the test stops compiling. `parse` stays the only constructor.
    trait NoFromString<Marker> {
        fn assert_not_implemented() {}
    }
    impl<T: ?Sized> NoFromString<()> for T {}
    impl<T: From<String>> NoFromString<u8> for T {}

    fn platform_failure_mentioning_the_key() -> keyring::Error {
        keyring::Error::PlatformFailure(SYNTHETIC_KEY.into())
    }

    fn platform_failure_mentioning_the_token() -> keyring::Error {
        keyring::Error::PlatformFailure(SYNTHETIC_TOKEN.into())
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
        assert_eq!(
            ApiKeyAccount::telemetry_headers().name(),
            "telemetry:otlp:headers"
        );
    }

    #[test]
    fn api_key_accounts_never_collide_with_connection_passwords() {
        let accounts = [
            ApiKeyAccount::anthropic(),
            ApiKeyAccount::openai(),
            ApiKeyAccount::openai_compatible(),
            ApiKeyAccount::telemetry_headers(),
        ];
        for (index, account) in accounts.iter().enumerate() {
            assert!(!account.name().starts_with("connection:"));
            assert!(accounts[index + 1..].iter().all(|other| other != account));
        }
        for id in ["abcdefghij", "klmnopqrst", "uvwxyz2345", "234567abcd"] {
            let token_name = McpTokenAccount::parse(id).unwrap().name();
            assert!(!token_name.starts_with("connection:"), "{token_name}");
            assert!(!token_name.starts_with("assistant:"), "{token_name}");
            assert!(
                accounts
                    .iter()
                    .all(|account| account.name() != token_name.as_str()),
                "{token_name}"
            );
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

    #[test]
    fn mcp_token_account_parse_accepts_only_ten_lowercase_base32_characters() {
        assert_eq!(BASE32.len(), 32);
        for start in 0..BASE32.len() {
            let id: String = BASE32.chars().cycle().skip(start).take(10).collect();
            let account = McpTokenAccount::parse(&id).unwrap();
            assert_eq!(account.id(), id);
            assert_eq!(McpTokenAccount::parse(account.id()), Some(account));
        }

        // `abcdefghé` is 10 bytes (é is two UTF-8 bytes) and 9 characters.
        let rejected = [
            "abcdefghi",
            "abcdefghijk",
            "",
            "ABCDEFGHIJ",
            "Abcdefghij",
            "abcdefghi=",
            "abcdefghi0",
            "abcdefghi1",
            "abcdefghi8",
            "abcdefghi9",
            "abcdefghiñ",
            "éééééééééé",
            "abcdefghé",
            "abcdefghi/",
            "abcdefghi:",
        ];
        for id in rejected {
            assert!(McpTokenAccount::parse(id).is_none(), "{id:?}");
        }
    }

    #[test]
    fn mcp_token_account_name_is_the_vault_account() {
        let account = McpTokenAccount::parse("abcdefgh23").unwrap();
        let name = account.name();
        assert_eq!(account.id(), "abcdefgh23");
        assert_eq!(name, "mcp:token:abcdefgh23");
        assert!(!name.starts_with("connection:"));
        assert!(!name.starts_with("assistant:"));
    }

    #[test]
    fn a_missing_mcp_token_reads_as_none() {
        assert!(
            read_mcp_token(Err(keyring::Error::NoEntry))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_stored_mcp_token_reads_back_as_a_secret() {
        let token = read_mcp_token(Ok(SYNTHETIC_TOKEN.to_owned()))
            .unwrap()
            .unwrap();
        assert_eq!(token.expose_secret(), SYNTHETIC_TOKEN);
        assert!(!format!("{token:?}").contains(SYNTHETIC_TOKEN));
    }

    #[test]
    fn removing_a_missing_mcp_token_succeeds() {
        removed_mcp_token(Ok(())).unwrap();
        removed_mcp_token(Err(keyring::Error::NoEntry)).unwrap();
    }

    #[test]
    fn mcp_token_vault_failures_omit_the_secret_and_the_account() {
        let errors = [
            read_mcp_token(Err(platform_failure_mentioning_the_token())).unwrap_err(),
            removed_mcp_token(Err(platform_failure_mentioning_the_token())).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.code, ErrorCode::Authentication);
            assert_eq!(
                error.message,
                "System vault unavailable. The token could not be read, saved or removed."
            );
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(SYNTHETIC_TOKEN), "{shown}");
            assert!(!shown.contains("abcdefghij"), "{shown}");
        }
    }

    #[test]
    fn mcp_token_account_has_no_from_string_constructor() {
        <McpTokenAccount as NoFromString<_>>::assert_not_implemented();
    }
}
