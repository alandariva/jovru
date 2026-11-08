# ADR-0033: Daily update check and signed self-install from GitHub releases

- **Status:** Accepted (2026-11-08; implemented, release key pending)
- **Date:** 2026-11-08
- **Authors:** Alan de Oliveira (with an AI agent)

## Context

Jykre Studio ships as a portable folder: a `.zip` for Windows and a
`.tar.gz` for Linux, drafted by `.github/workflows/release.yml` on a `v*`
tag and published by hand on GitHub. Each archive holds one folder with the
executable, `LICENSE`, `README.md`, the SQL catalogs (`catalog/`, read at
start from next to the executable, [ADR-0021](0021-sql-catalogs-read-at-run-time.md))
and, on Linux, the desktop entry, its icon and `install.sh`. Nothing tells a
user that a newer version exists; updating means downloading the archive
again and replacing the folder by hand.

Forces:

- [Performance](../standards/performance.md) principle 1 allows the network
  only for explicit actions, and for telemetry only when the user turns it
  on. An update check that runs by itself is a new exception.
- An updater that replaces the executable is the most powerful code path in
  the application: whoever can publish an archive it accepts runs code on
  every user's computer. A checksum published next to the archive proves
  nothing if the release itself is replaced.
- Every published release so far is marked **Pre-release**, so GitHub's
  `/releases/latest` endpoint answers 404.
- The application writes user data only in its data folder; an update must
  never touch it, nor files the user put next to the executable.
- Restarting must not skip the questions that closing the window asks
  (SQL not saved, an open transaction).

## Decision

- **Check** GitHub's release list
  (`GET https://api.github.com/repos/alandariva/jykre-studio/releases`)
  **once a day**, **on by default**, with **Preferences › About › Updates ›
  Check for updates automatically** to turn it off and **Check now** for a
  manual check. The first automatic check waits 30 s after the window opens
  and runs only if the last one is older than 24 h; it never runs on the UI
  thread or during start-up. Debug builds never check by themselves. The
  request is anonymous: no token, cookie or identifier, and a `User-Agent`
  of `JykreStudio/<version>`. GitHub sees the address, the version and the
  time. This amends performance principle 1.
- **Pre-releases count as updates.** Drafts are never seen (the request
  carries no token). The highest semantic version strictly above the
  running one is offered.
- **Nothing is downloaded or replaced without a click.** A newer version
  shows a badge in the status bar and in About; **Download and install**
  shows progress and can be cancelled; **Restart now** or **Later** follows.
- **Signatures are mandatory.** `release.yml` signs each archive with
  [zipsign](https://github.com/Kijewski/zipsign) (ed25519ph) in a job of its
  own, with the private key in the `ZIPSIGN_PRIVATE_KEY` secret. The public
  key is compiled into the application (`assets/update/release-key.pub`).
  The signature's context is the archive's file name, which the application
  computes itself from the version (`jykre-studio-v<version>-<os>-x86_64.<ext>`),
  so an older signed archive cannot be passed off as a newer one. An archive
  that does not verify is never extracted; the application offers only
  **Download manually** (the release page).
- **Self-install only in a release layout:** a release build on Linux or
  Windows x86_64, an executable named `jykre`/`jykre.exe` with
  `catalog/` next to it, a folder the user can write to, and the
  platform's archive in the release. Otherwise the update is offered as a
  link only.
- **Install by swapping entries, with rollback.** After verification the
  archive is extracted into `.jykre-update/` next to the executable (same
  file system). Each top-level entry of the archive's folder replaces the
  entry of the same name, the executable last; replaced entries are moved
  into `.jykre-update/trash-<id>/` and removed (on Windows the running
  executable is removed at the next start). A failure undoes every step in
  reverse order. Entries the archive does not hold, and the data folder,
  are never touched. Symbolic links and unsafe names in the archive are
  refused.
- **Restart through GPUI** (`App::set_restart_path`, `App::restart`), after
  the same confirmations as closing the window, so exit writes run.
- **Library:** `self_update` 1.3.3 without default features, with
  `github`, `async`, `signatures`, `archive-tar`, `compression-tar-gz`,
  `archive-zip` and `compression-zip-deflate`: its release listing (with our
  own `reqwest` 0.13 client), `verify_signature` and `Extract`. Its
  high-level `Update`, `Download`, `MoveAll` and `restart` are not used
  (below). Its `rustls` feature is never turned on: through `reqwest` 0.13
  it compiles `aws-lc-rs`; `deny.toml` bans `aws-lc-rs` and `aws-lc-sys`.
- **Network:** HTTPS only, rustls with ring and the system's trust store.
  The release list does not follow redirects. Downloading an asset follows
  at most 3 redirects, only to `https://objects.githubusercontent.com` and
  `https://release-assets.githubusercontent.com` on port 443: a narrow
  exception to "redirects are not followed" in
  [Security](../standards/security.md). Archives are capped at 256 MiB.
  Timeouts are in [Reliability](../standards/reliability.md#timeouts).

## Alternatives considered

| Alternative | Pros | Cons | Why not |
| --- | --- | --- | --- |
| Notify only, with a link to the release page | No new dependency; nothing replaced | Every update stays manual | The maintainer asked for installation |
| Check off by default (opt-in, like telemetry) | Keeps performance principle 1 as it is | Most users would never learn of a fix | The maintainer chose on by default (2026-11-08) |
| `self_update`'s high-level `Update` | Fewest lines | Prefers the same minor version on 0.x, prints to stdout and reads stdin, matches assets by substring (the `.sha256` too), its bundle mode removes the user's files and replaces only one path | Does not fit the portable folder |
| `self_update`'s `Download` and `MoveAll` | Less of our code | In 1.3.3 `Download` builds its own client (any redirect, no read timeout); `MoveAll` cannot roll back a new directory | Our download and swap are short and testable |
| Velopack | Installer, deltas, mature updater | Changes distribution to an installer, needs `vpk` (.NET 8) in CI and a hook in `main` | Replaces the portable folder and `install.sh` |
| `dist` (cargo-dist) with `axoupdater` | Generated installers and updater | Replaces the whole release pipeline | Too large a change for the gain |
| `cargo-packager-updater` | Tauri-like updater | Bound to its own package formats; no release since 2025-07 | Health |
| SHA-256 only (the `.sha256` file or GitHub's digest) | No key to keep | Proves integrity, not origin: whoever replaces the release replaces the checksum | Code that installs code needs an authentic origin |
| minisign or Sigstore signatures | Widely known | A separate signature file per asset to fetch and match | zipsign keeps the signature inside the archive and is what `self_update` verifies |

## Consequences

- Positive: users learn of a new version within a day and install it with
  two clicks; an archive the maintainer's key did not sign is never
  installed; a failed or cancelled install leaves the running version as
  it was.
- Negative or costs:
  - 18 new crates (`self_update`, `self-replace`, `tar`, `filetime`,
    `xattr`, `urlencoding`, `zipsign-api`, `ed25519-dalek`, `ed25519`,
    `signature`, `curve25519-dalek` and its derive crate, `pkcs8`, `der`,
    `spki`, `const-oid`, `base64ct`, `fiat-crypto`); `reqwest` 0.13 also
    compiles `blocking`, `json`, `http2` and `stream` through feature
    unification.
  - A daily request to GitHub by default, which reveals the address and the
    version.
  - A signing key to keep: if it is lost, updates must be installed by hand
    once; if it leaks, the key must be rotated (ship the new key, sign with
    both, then drop the old one).
  - The first release with the updater (0.2.0) cannot update itself from
    0.1.0; the first automatic update is the release after it.
  - The replaced executable (about 40 MB) stays in `.jykre-update/` until
    the next start on Windows.
- Required: every release from 0.2.0 on, release candidates included, is
  signed; `release.yml` verifies the signatures with the committed public
  key before drafting; a change to the redirect hosts or to the archive
  layout updates this ADR, [Security](../standards/security.md) and the
  tests.
- Forbidden: installing an archive that did not verify; sending a token or
  an identifier to GitHub; following a redirect outside the two hosts;
  writing outside the executable's folder.

## How to validate

- Unit tests: selection of the release (pre-releases, order, older and
  equal versions, missing asset), the redirect policy, the preferences and
  the schedule; signatures made in the test with a test key (the right key
  passes; a wrong key, a renamed file, a changed byte and an unsigned
  archive fail); extraction and validation of the staged folder; the swap
  with a failure injected at each step leaving the folder as it was.
- A loopback HTTP stub serves a paginated release list, errors, a redirect,
  a stalled body and a signed archive, and the whole install runs into a
  temporary folder.
- `cargo tree -i aws-lc-rs` prints nothing; `cargo deny --locked check`
  passes with the bans.
- On Windows and Linux, signed pre-releases `v0.2.0-rc.1` and `-rc.2`: the
  first finds the second, installs it and restarts; then a read-only
  folder, no network, a cancelled download, a wrong signature, and SQL not
  saved or an open transaction before the restart.

## Implementation notes

Implemented on 2026-11-08 (`jykre::update`, `src/app/updates.rs`, the
`sign` and `verify-windows` jobs of `release.yml`).

- The committed `assets/update/release-key.pub` is a placeholder whose
  private key was discarded, so nothing verifies against it: the
  maintainer replaces it with the real key before the `v0.2.0` tag
  (`assets/update/README.md`).
- `self_update`'s `ReleaseList` reads no `draft` or `prerelease` field;
  drafts stay out because no token is sent, and pre-releases are in on
  purpose.
- On Linux the running process's own path follows the replaced file after
  the swap, so the executable to restart is recorded before it.
- If GPUI cannot start the new version (no `bash` on Linux, no PowerShell
  on Windows), the window still closes and the update runs at the next
  start.
