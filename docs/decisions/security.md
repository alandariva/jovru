# Decisions: Security

Why this area is built as it is. Each entry records what was decided, why, and which alternatives lost. The rules themselves are in [Security](../standards/security.md). Entries are numbered `D-NNNN`, and the numbers are stable.

<a id="d-0004"></a>
## D-0004: Operating system vault for credentials, with no file fallback

*Accepted 2026-11-02 (recorded for a decision already in effect) · implemented*

**Decision.** Remembered passwords go to the OS vault through `jovru-secrets` and never to a file; remembering is optional, and without it or when the vault is unavailable the password lives in memory for the session and the UI says why. Storage names and rules are in [Credentials](../standards/security.md#credentials).
**Why.** Users connect to production databases, and a remembered password must survive a restart without ever being written to the application's files, logs or error messages. The vault ties the secret to the OS account. The cost is that on a system without a vault the user retypes the password each session.
**Rejected.** An encrypted file with a key in the binary: works everywhere, but it is plain text with extra steps and no real protection. A master password: portable, but extra friction and a new secret to manage, not justified for a desktop client.

<a id="d-0033"></a>
## D-0033: Daily update check and signed self-install from GitHub releases

*Accepted 2026-11-08 · implemented (the committed release key is the real one)*

**Decision.** The application checks GitHub's release list once a day, on by default (off in **Preferences › About › Updates**), offers a newer version including pre-releases, and installs it only after the user's click, and only when the archive verifies against the compiled-in zipsign public key. The install swaps entries in the portable folder with rollback and restarts through GPUI. The binding detail is in [Updates](../reference/updates.md#updates). It is the exception to principle 1 of [Performance](../standards/performance.md#principles), which allows the network only for explicit actions and for opt-in telemetry.
**Why.** Jovru ships as a portable folder (a `.zip` for Windows, a `.tar.gz` for Linux, drafted by `release.yml` on a `v*` tag), and nothing told a user that a newer version existed. An updater that replaces the executable is the most powerful code path in the application: whoever can publish an archive it accepts runs code on every user's computer, and a checksum published beside the archive proves nothing if the release itself is replaced. The maintainer chose the check on by default on 2026-11-08, and asked for installation, not only a notice. Costs: 18 new crates (and `reqwest` 0.13 compiling `blocking`, `json`, `http2` and `stream` by feature unification), a daily request that reveals the address and the version, and a signing key to keep. The first release with the updater (0.2.0) could not update itself from 0.1.0. The replaced executable (about 40 MB) stays in `.jovru-update/` until the next start on Windows.
**Rejected.** Notify only, with a link: nothing replaced, but every update stays manual. Check off by default, like telemetry: keeps principle 1, but most users would never learn of a fix. `self_update`'s high-level `Update`: prefers the same minor version on 0.x, prints to stdout and reads stdin, matches assets by substring (the `.sha256` too), and its bundle mode removes the user's files and replaces only one path. Its `Download` and `MoveAll`: in 1.3.3 `Download` builds its own client (any redirect, no read timeout) and `MoveAll` cannot roll back a new directory. Velopack: an installer and deltas, but it changes distribution, needs `vpk` (.NET 8) in CI and a hook in `main`, and replaces the portable folder and `install.sh`. `dist` (cargo-dist) with `axoupdater`: replaces the whole release pipeline. `cargo-packager-updater`: bound to its own package formats and no release since 2025-07. SHA-256 only: proves integrity, not origin. minisign or Sigstore: a separate signature file per asset to fetch and match, while zipsign keeps the signature inside the archive and is what `self_update` verifies.
