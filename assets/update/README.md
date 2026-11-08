# Release signing key

`release-key.pub` is the raw 32-byte ed25519 public key that an update
archive must be signed with before Jykre Studio installs it
([ADR-0033](../../docs/adr/0033-update-check-and-signed-self-install.md)).
The release workflow's `sign` job signs with the matching private key, kept
only as the `ZIPSIGN_PRIVATE_KEY` secret, and verifies the result against this
file before drafting a release.

> **Placeholder.** The key committed now was generated with its private key
> discarded, so nothing can be signed for it and no update installs. Before
> the `v0.2.0` tag the maintainer replaces it:
>
> ```bash
> cargo install --locked zipsign --version 0.2.2
> zipsign gen-key jykre-release.key jykre-release.pub
> cp jykre-release.pub assets/update/release-key.pub
> base64 -w0 jykre-release.key   # the ZIPSIGN_PRIVATE_KEY secret
> ```
>
> Keep `jykre-release.key` offline and backed up; never commit it.
