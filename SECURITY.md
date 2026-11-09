# Security policy

## Supported versions

Jykre Studio is an early prototype. Only the latest commit on `main` and the
latest release receive security fixes.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Use GitHub's
private reporting instead: open the repository's **Security** tab and choose
**Report a vulnerability**.

Include the affected version or commit, the platform, steps to reproduce and
the impact you observed. Do not include real passwords, tokens or production
data; synthetic values are enough.

You should receive an acknowledgement within 7 days. Once the issue is
confirmed, a fix and an advisory are prepared, and you are credited unless you
prefer otherwise.

## Scope

Areas that are especially relevant:

- Credential handling: passwords must stay in memory or in the OS vault and
  never reach SQLite, JSON, logs or error messages.
- Databricks tokens, which are resolved through the CLI and kept in memory.
- TLS modes for PostgreSQL and MySQL connections.
- The local SQLite files (`config.db`, `metadata/*.db`), which hold
  non-sensitive configuration and object names but are not encrypted.
- SQL sent to servers: the application runs the SQL you write; previews quote
  identifiers.
- Updates: an archive is installed only when it verifies against the release
  key compiled into the application (`assets/update/release-key.pub`); its
  download follows redirects only to GitHub's download hosts. A way to make
  the application install an archive that key did not sign is a
  vulnerability.

The test fixture password `jykre-test` in `docker-compose.test.yml` is public on
purpose and only binds to `127.0.0.1`; it is not a vulnerability.

## Dependency audit

`cargo deny` checks advisories, licenses and sources in CI and weekly. Accepted
exceptions are listed in `deny.toml` with a reason and a review date.
