# Update check and self-install

How Jovru finds a newer version and installs it: what the check sends, what a
release must carry to be installed, what is written and when the application
offers only the release page. The flow through the code is in
[Architecture](architecture.md#update); the reasoning is in
[Decisions: Security](../decisions/security.md#d-0033); the timeouts are in
[Reliability](../standards/reliability.md#timeouts).

## Updates

The application checks GitHub's release list for a newer version and, after
the user's click, installs it
(D-0033).

- **Sent:** once a day by default (off in **Preferences › About ›
  Updates**), or on **Check now**, one `GET` of
  `https://api.github.com/repos/alandariva/jovru/releases` with
  `User-Agent: Jovru/<version>`. GitHub sees the computer's address,
  the version and the time. **Never sent:** a token, a cookie, a
  `Referer`, an identifier of the user or the computer, the operating
  system. Downloads happen only after **Download and install**; nothing is
  downloaded or replaced without that click.
- **When it checks.** The first automatic check waits 30 s after the window
  opens and runs only if the last one is older than 24 h; it never runs on
  the UI thread or during start-up, and debug builds never check by
  themselves. The request is anonymous and sets no token. Timeouts and the
  wait after a failed check are in [Reliability](../standards/reliability.md#timeouts).
- **Which release.** Pre-releases count as updates (every release so far is
  marked **Pre-release**, so GitHub's `/releases/latest` answers 404);
  drafts are never seen, because no token is sent. The highest semantic
  version strictly above the running one is offered.
- **Signature, required.** An archive is installed only when it verifies
  against a public key compiled in (`assets/update/release-key.pub`,
  `jovru::update::keys`), signed with zipsign (ed25519ph). The context of
  the signature is the archive's file name, which the application builds
  from the version, never from GitHub's answer
  (`jovru-v<version>-<os>-x86_64.<ext>`), so an older signed archive cannot
  pass for a newer one. A checksum next to the archive is not trusted:
  whoever can replace a release can replace it. An archive that does not
  verify is never extracted; the application offers only the release page.
  Every release from 0.2.0 on, release candidates included, is signed.
- **Key custody.** The private key exists only offline with the maintainer
  and as the `ZIPSIGN_PRIVATE_KEY` secret of the release workflow's `sign`
  job, which verifies what it signed with the committed public key before a
  release is drafted. To rotate it, ship the new public key beside the old,
  sign a release with both, then drop the old one. A leaked key is rotated
  at once; a lost key means users update by hand once.
- **Redirects, the one exception.** The release list follows none. A
  download starts at the repository's API asset address
  (`…/releases/assets/<number>`, checked) and follows at most 3 redirects,
  only over HTTPS on port 443 to `objects.githubusercontent.com` or
  `release-assets.githubusercontent.com`, without a user or a password.
  Those addresses carry a signed token in their query, so no address is
  logged or put in an error message.
- **What is written.** The archive (at most 256 MiB) goes to
  `.jovru-update/` next to the executable; it is unpacked only after its
  signature verifies; the unpacked folder must hold one folder with the
  executable and `catalog/`, and no symbolic link or unsafe name. The swap
  replaces each top-level entry of the archive's folder with the entry of
  the same name, the executable last, in the executable's folder; what it
  replaces is moved into `.jovru-update/trash-<id>/` and removed (on
  Windows the running executable is removed at the next start), and a
  failure undoes every step in reverse order. Entries the archive does not
  hold, the data folder and the user's own files are never touched.
- **Only where it ships.** Self-install needs a release build on Linux or
  Windows x86_64, an executable named `jovru`/`jovru.exe` with `catalog/`
  next to it, a folder the user can write to and the platform's archive in
  the release. A development build, another platform, a copy not in the
  release's folder or a folder the user cannot write to is offered the
  release page instead; the application never asks for elevated rights.
- **Restart.** After the install, **Restart now** goes through GPUI
  (`App::set_restart_path`, `App::restart`) after the same confirmations as
  closing the window, so exit writes run; the executable to restart is
  recorded before the swap, because on Linux the process's own path follows
  the replaced file. If GPUI cannot start the new version, the window still
  closes and the update runs at the next start.
- **Forbidden.** Installing an archive that did not verify; sending a token
  or an identifier to GitHub; following a redirect outside the two hosts;
  writing outside the executable's folder. A change to the redirect hosts or
  to the archive layout updates this section and the tests (selection of the
  release, the redirect policy, signatures made with a test key, the staged
  folder and the swap with a failure injected at each step).
- **Library.** `self_update` is used only for the release list, signature
  verification and unpacking, with our own `reqwest` client; its `Update`,
  `Download`, `MoveAll` and `restart` are not used
  ([Stack and dependencies](../standards/stack-and-dependencies.md)). Its `rustls`
  feature stays off: it would compile `aws-lc-rs`, which `deny.toml` bans.

## Tests

- Updates are tested without reaching GitHub (`jovru::update::tests`): a
  server on `127.0.0.1` serves the release list, the redirect and the
  archive, and archives are signed in the test with a key made from a fixed
  seed. The committed release key never signs anything in a test. The whole
  flow against real signed pre-releases is a manual check on Windows and
  Linux, recorded with the release
  ([D-0033](../decisions/security.md#d-0033)).
