//! Native credential vault. There is deliberately no plaintext file fallback.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::{
    ConnectionId, Credentials, DriverError, DriverResult, ErrorCode, SecretString, async_trait,
};
use secrecy::ExposeSecret;

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
    keyring::Entry::new("JykreStudio", &format!("connection:{id}:password"))
        .map_err(|_| unavailable())
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
