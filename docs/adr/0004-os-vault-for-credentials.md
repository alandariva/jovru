# ADR-0004: Operating system vault for credentials, with no file fallback

- **Status:** Accepted (recorded 2026-11-02 for a decision already in effect)
- **Date:** 2026-11-02

## Context

Users connect to production databases. A remembered password must survive a
restart without ever being written to the application's files, logs or error
messages.

## Decision

- Remembered passwords go to the OS vault through `jykre-secrets` (`keyring`
  with Windows Credential Manager, macOS Keychain or the Linux Secret
  Service), under service `JykreStudio`, account
  `connection:<connection-id>:password`.
- Remembering is optional. Without it, or when the vault is unavailable, the
  password lives in memory for the session and the UI says why.
- There is no fallback to a file and no home-made encryption.
- Secrets are `SecretString`; types holding them are not serializable.

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Encrypted file with a key in the binary | Works everywhere | Plain text with extra steps | Not real protection |
| Master password | Portable | Extra friction, a new secret to manage | Not justified for a desktop client |

## Consequences

- Positive: secrets are protected by the OS account; nothing sensitive in
  local files.
- Negative: on a system without a vault the user retypes the password each
  session.
- Required: vault calls run in `spawn_blocking`; failures are reported
  without the secret.
