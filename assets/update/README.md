# Release signing key

`release-key.pub` is the raw 32-byte ed25519 public key that an update
archive must be signed with before Jovru installs it
([D-0033](../../docs/decisions/security.md#d-0033)).
The release workflow's `sign` job signs with the matching private key, kept
only as the `ZIPSIGN_PRIVATE_KEY` secret, and verifies the result against this
file before drafting a release.

> **Replacing the key.** To make a new pair, for example to rotate it (see
> `src/update/keys.rs` for the safe order):
>
> ```bash
> cargo install --locked zipsign --version 0.2.2
> zipsign gen-key jovru-release.key jovru-release.pub
> cp jovru-release.pub assets/update/release-key.pub
> base64 -w0 jovru-release.key   # the ZIPSIGN_PRIVATE_KEY secret
> ```
>
> Keep `jovru-release.key` offline and backed up; never commit it.
