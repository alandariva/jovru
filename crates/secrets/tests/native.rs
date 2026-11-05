#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jykre_core::{ConnectionId, SecretString};
use jykre_secrets::{SecretStore, SystemSecretStore};
use secrecy::ExposeSecret;

#[tokio::test]
#[ignore = "requires an unlocked native OS vault; cargo test -p jykre-secrets --test native -- --ignored"]
async fn native_vault_round_trip_with_disposable_synthetic_credentials() {
    let id = ConnectionId::new();
    let store = SystemSecretStore;
    store
        .set(
            id,
            SecretString::from("jykre-synthetic-vault-fixture".to_owned()),
        )
        .await
        .unwrap();
    let loaded = store.get(id).await;
    // Remove the disposable entry before assertions, also if the read failed.
    store.delete(id).await.unwrap();
    assert_eq!(
        loaded.unwrap().password.unwrap().expose_secret(),
        "jykre-synthetic-vault-fixture"
    );
    assert!(store.get(id).await.unwrap().password.is_none());
}
