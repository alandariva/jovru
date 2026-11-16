#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
use jovru_core::{ConnectionId, SecretString};
use jovru_secrets::{ApiKeyAccount, ApiKeyStore, SecretStore, SystemSecretStore};
use secrecy::ExposeSecret;

#[tokio::test]
#[ignore = "requires an unlocked native OS vault; cargo test -p jovru-secrets --test native -- --ignored"]
async fn native_vault_round_trip_with_disposable_synthetic_credentials() {
    let id = ConnectionId::new();
    let store = SystemSecretStore;
    SecretStore::set(
        &store,
        id,
        SecretString::from("jovru-synthetic-vault-fixture".to_owned()),
    )
    .await
    .unwrap();
    let loaded = SecretStore::get(&store, id).await;
    // Remove the disposable entry before assertions, also if the read failed.
    SecretStore::delete(&store, id).await.unwrap();
    assert_eq!(
        loaded.unwrap().password.unwrap().expose_secret(),
        "jovru-synthetic-vault-fixture"
    );
    assert!(
        SecretStore::get(&store, id)
            .await
            .unwrap()
            .password
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires an unlocked native OS vault; cargo test -p jovru-secrets --test native -- --ignored"]
async fn native_vault_round_trip_of_a_synthetic_api_key() {
    // A fixed account: a developer's real OpenAI-compatible key stored there
    // would be replaced and then removed by this test.
    let account = ApiKeyAccount::openai_compatible();
    let store = SystemSecretStore;
    ApiKeyStore::set(
        &store,
        account,
        SecretString::from("sk-jovru-synthetic-vault-fixture".to_owned()),
    )
    .await
    .unwrap();
    let loaded = ApiKeyStore::get(&store, account).await;
    // Remove the disposable entry before assertions, also if the read failed.
    ApiKeyStore::delete(&store, account).await.unwrap();
    assert_eq!(
        loaded.unwrap().unwrap().expose_secret(),
        "sk-jovru-synthetic-vault-fixture"
    );
    assert!(ApiKeyStore::get(&store, account).await.unwrap().is_none());
    // Removing an entry that is already gone is not an error.
    ApiKeyStore::delete(&store, account).await.unwrap();
}
