# Security

## Reporting a vulnerability

Do not open a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/herdem-herdem/guvercin-desktop/security/advisories/new)
for anything that affects the confidentiality or integrity of a user's mail,
credentials or local databases.

Include what you did, what happened, and the platform and commit you were on. A
first reply should come within a week. There is no bounty programme.

## What the threat model actually covers

guvercin is a single-user desktop application. Everything below is a statement
about the current code, not an aspiration.

**Covered.**

- Data at rest against another *local user account* on the same machine, and
  against anyone reading the disk without the master key. Every SQLite database
  is opened through SQLCipher with a key derived per database from a 256-bit
  master key; cached attachments, inline assets and avatars are sealed with
  XChaCha20-Poly1305 in 64 KiB chunks, each chunk carrying its own
  authentication tag (`rust-backend/src/crypto.rs`).
- Network confidentiality. IMAP runs over TLS via `native-tls`; SMTP (lettre)
  and all HTTP (reqwest, sqlx) use rustls. OAuth uses the authorization code
  flow with PKCE in the system browser; the token exchange happens in the
  backend and the app never sees or stores a Google password.
- Script execution from mail. Before a message is written into the reading
  pane, `frontend/src/utils/externalLinks.js` removes `script`, `iframe`,
  `object`, `embed`, `applet`, `form` and `link[rel=import]` elements, strips
  every `on*` attribute, and neutralises `javascript:` URLs. Links are rewritten
  to `data-external-href` and opened through a confirmation prompt rather than
  navigating the pane.
- Tracking pixels. Remote images, remote `url()` backgrounds in inline styles
  and remote stylesheets are withheld from the reading pane; their URLs are
  parked on the element and only restored when the reader clicks "load
  images". `cid:` and `data:` assets, which cost the sender no request, are
  untouched. The default is per-message prompting; Settings → Remote Images
  can make it always-load or never-load.
- Local network exposure. The HTTP API binds `127.0.0.1:0` and is an in-process
  boundary, not a service. Nothing listens on an external interface.

**Not covered — read this before trusting the encryption claim.**

- **The master key is stored unprotected in the user's profile**, at
  `<local app data>/com.guvercin.app/master.key`. It is not wrapped by the OS
  keychain and not protected by a passphrase. On Unix the file is mode 0600, so
  other local users cannot read it; on Windows it inherits the per-user ACL of
  the local app data directory. Anything running *as that user* — including
  malware in the user's session — can read the key and decrypt every database.
  If you need protection against a stolen unlocked machine or a compromised
  user session, SQLCipher here does not give it to you. Full-disk encryption
  (FileVault, BitLocker, LUKS) is still your responsibility. Moving the key
  into the platform keychain is open work.
- **The shipped Google OAuth client secret is public.** It is compiled into the
  binary (`rust-backend/src/oauth.rs`) because an installed desktop app has
  nowhere to hide it; Google's desktop client type is designed on that
  assumption and PKCE is what actually protects the flow. Treat the value as
  published, not secret.
- **Withholding remote images hides the fact that you opened a message, not
  your IP.** Once you click "load images" the request goes out from your
  machine directly — there is no relay. The `proxy-image` endpoint exists but
  is used only by the PDF/print export, and it would not help anyway: it runs
  on the same machine.
- **Mail HTML is filtered by a denylist, in an iframe that is not a real
  sandbox.** The reading pane iframe carries
  `sandbox="allow-same-origin allow-scripts"`, a combination that grants no
  meaningful isolation — `allow-scripts` is present because the parent needs
  to attach listeners inside the frame. Safety therefore rests entirely on the
  hand-written filter described above, which removes known-dangerous elements
  and attributes rather than allowing only known-safe ones. A gap in that
  filter means script execution in the app's origin. This is the highest-value
  place to look for a bug, and reports here are especially welcome.
- Deleting `master.key` destroys access to every existing database. There is no
  recovery path and no escrow. This is by design and is not a bug.

## Changes that need review

Any pull request touching `rust-backend/src/crypto.rs`,
`rust-backend/src/keystore/`, `rust-backend/src/db.rs`,
`rust-backend/src/oauth.rs`, or the HTML sanitisation path must spell out its
security implications in the description.
