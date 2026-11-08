//! The public keys an archive must be signed with to be installed.
//!
//! `assets/update/release-key.pub` holds the raw 32 bytes of an ed25519
//! public key, as `zipsign gen-key` writes it. Its private key is kept by
//! the maintainer and reaches only the release workflow's `sign` job, as
//! the `ZIPSIGN_PRIVATE_KEY` secret. To rotate it, add the new key here,
//! sign with both for a release, then remove the old one
//! ([Security](../../docs/standards/security.md#updates)).

/// Any one of these keys may sign an archive.
pub const RELEASE_KEYS: &[self_update::VerifyingKey] =
    &[*include_bytes!("../../assets/update/release-key.pub")];

// `verify_signature` accepts any archive when given no key.
const _: () = assert!(!RELEASE_KEYS.is_empty());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_keys_are_ed25519_public_keys() {
        for key in RELEASE_KEYS {
            assert!(self_update::zipsign_api::VerifyingKey::from_bytes(key).is_ok());
        }
    }
}
