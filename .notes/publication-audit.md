# Publication audit before the first push — 2026-10-21

## What

A privacy, licensing and repository-hygiene pass over everything git would publish, done offline
(no phone, no commit) together with the final documentation pass. Result: **publishable after the
steps in [Remaining steps](#remaining-steps-before-the-first-push)**. Nothing secret or personal is
left in the working tree's publishable files, but the existing `wip/checkpoint-2026-10-21` commit is
not clean, so the first public commit must be a fresh one.

## How

- **File set:** `git ls-files` plus `git ls-files --others --exclude-standard`: 134 paths, of which
  128 exist (the other 6 are deleted in the working tree but still in the index: the five per-crate
  `Cargo.lock` files and `gui/smartconnect-gui.desktop`; `git add -A` drops them).
- **Privacy scan:** a throwaway script over those files for IPv4/IPv6 addresses, MACs, UUIDs,
  emails, phone-number-like digit runs, SSIDs, serial-like tokens, and base64/hex blobs above an
  entropy threshold, plus greps for `/home/`, `/mnt/`, the owner's names, the real LAN prefix, the
  Windows PC's hostname, the developer's build-directory names and `evidence-2026-10-20`. Every hit
  was read by hand, and the older notes were read in full for notification/SMS text and device
  details.
- **History:** `git show`/`git grep` on `HEAD` (the single WIP commit) for what the working tree no
  longer has.
- **Licenses:** `cargo metadata --format-version 1 --manifest-path Cargo.toml --locked` (full graph,
  332 third-party packages) and again with `--filter-platform x86_64-unknown-linux-gnu` (304 that
  are actually built on Linux); every package's `license` field grouped and checked.
- **Size and type:** `file --mime` and sizes of every publishable file, and the index's file modes.

## Findings and fixes

| Finding | Where | Action |
|---|---|---|
| The Windows PC's hostname | `.notes/rung4-pairing.md`, `.notes/rung5-session-and-pairing-commit.md` | Replaced with "the Windows PC". |
| A real-looking Indian mobile number (in four spellings) as SMS test data | `src/sms.rs` tests | Replaced with the fictional `+1 555 555 0123` range; `cargo test --release -p smartconnect-linux --lib sms` passes (3 tests). The obviously fake `+910000000000`/`+911234567890` were kept. |
| A quoted line of decompiled code | `.notes/notifications-and-desktop-ui.md` | Rephrased in own words. |
| A developer build-directory path | `.notes/packaging-and-workspace.md` | Reworded without the path. |
| References to `evidence-2026-10-20/` (private research folder) | 11 notes, comments in 15 source files | Explained by a top line in `.notes/index.md`; code comments left as they are (see [Why](#why)). |
| Owner handoff note with machine-specific state (local build directories, what was installed on this PC, agent remarks) | `.notes/handoff-2026-10-21.md` (untracked, appeared during this pass) | Not for publication: listed in the index as local-only; remove before the first commit (step 1). |
| `"author": "Rudra"` | `noctalia-plugin/smart-connect/manifest.json` | Left for the owner: keep, or use the GitHub handle. The LICENSE copyright line is intended. |
| Commit identity is the owner's full name and Gmail address | `git config user.name/user.email` (seen on `HEAD`) | Owner decision before committing (step 2). |
| `packaging/install.sh` and `uninstall.sh` are tracked as `100644` (the checkout is on NTFS, `core.fileMode=false`), so `./packaging/install.sh` would fail with "Permission denied" after a clone | git index | Set the executable bit in the index (step 3). |
| The WIP commit contains `noctalia-plugin/smart-connect/settings.json` (a developer build path), `desktop/__pycache__/*.pyc` (embeds the absolute path of the checkout, including the Windows user folder), the hostname and phone numbers above, and the per-crate lock files | `HEAD` of `wip/checkpoint-2026-10-21` | Both files are already `git rm --cached` and git-ignored; the history itself must not be pushed: publish a fresh orphan commit (step 3). |

Checked and fine:

- **Addresses:** only placeholders (`192.168.1.20`, `192.168.1.0/24`, `192.168.1.30/.35`) and test
  fixtures (`10.0.0.x`, `172.17.0.1`, `100.64.0.x`, `203.0.113.9`, `8.8.8.8`); no address from the
  real LAN. The one MAC is `00:11:22:33:44:55` in `tests/handshake.rs`.
- **Keys and ids:** the only long hex/base64 values are the documented dummy test vector in
  `docs/protocol/pairing-and-discovery.md` (keys `00 01 … 1f`) and `src/discovery.rs`; the UUID is
  the Bluetooth base UUID. Test device ids are `phone-nonce-id`/`phone-real-id`.
- **Personal text:** examples use `com.example.*`, "See you at 6", "On my way", `+15555550123`,
  `<own number>`. Notes name apps (WhatsApp, Clock) but quote no message or notification text,
  contact names or SSIDs (the Hotspot SSID is the `SmartConnect-XXXX` pattern).
- **Paths:** `/home/user/…` and `/run/user/1000/…` in API examples are generic; no other home,
  `/mnt` or Windows paths.
- **Ignored, never published:** `CLAUDE.local.md`, `.cargo/config.toml` (absolute target dir),
  `noctalia-plugin/smart-connect/settings.json` (live plugin settings), `__pycache__/`, `*.log`,
  `config.json`, `*KEEP-PRIVATE*`, captures, APKs, DLLs, makepkg output.
- **Size and type:** no binaries or generated files in the publishable set besides `Cargo.lock`
  (84 KB, kept on purpose for reproducible binaries). The largest source files are about 75 KB. The
  icon `packaging/dev.smartconnect.Linux.svg` is 1 KB of hand-written SVG.

## Dependency licenses

All 332 third-party crates are permissively licensed; none is GPL, AGPL, or without a license field.

| License (as declared) | Crates |
|---|---|
| MIT OR Apache-2.0 (in its spellings) | 223 |
| MIT | 87 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 |
| Unlicense OR MIT (`memchr`, `aho-corasick`); MIT OR Apache-2.0 OR Zlib | 2; 2 |
| ISC (`rustls-webpki`, `untrusted`); Apache-2.0 OR ISC OR MIT (`rustls`) | 2; 1 |
| BSD-2-Clause OR Apache-2.0 OR MIT (`zerocopy`, `zerocopy-derive`) | 2 |
| BSD-2-Clause (`bluer`), BSD-3-Clause (`subtle`), MIT OR BSD-3-Clause (`if-addrs`) | 1 each |
| Apache-2.0 (`gethostname`), Apache-2.0 AND ISC (`ring`), Apache-2.0 WITH LLVM-exception (`target-lexicon`, build-time) | 1 each |
| (MIT OR Apache-2.0) AND Unicode-3.0 (`unicode-ident`) | 1 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later (`r-efi`, UEFI only, never built on Linux) | 1 |

All are compatible with releasing this project under MIT. Native libraries are linked dynamically
and found through pkg-config: GStreamer and its plugins, GLib, GTK 4, libadwaita, Pango, Cairo and
libnice (LGPL-2.1+, some dual MPL), `libdbus-1` (AFL-2.1 OR GPL-2.0+, not vendored: `libdbus-sys`
has only its `pkg-config` feature), Wayland (MIT). GPL code is only ever a separate program or a
runtime-loaded GStreamer plugin from the user's system: `wvkbd` (GPL-3.0), `x264enc`
(gst-plugins-ugly), `avdec_h264` (gst-libav/FFmpeg). Nothing GPL is linked or shipped.

Optional for binary packages: ship third-party license texts (for example generated with
`cargo about`), since MIT, BSD and ISC ask for the notice to travel with binaries.

## Remaining steps before the first push

Run from the checkout. Nothing here was done by this pass.

1. ~~**Remove the local handoff note**~~ Done 2026-10-21: it now lives outside the checkout
   (`../smartconnect-handoff-2026-10-21.md`, referenced from the git-ignored `CLAUDE.local.md`),
   and its index line is gone.
2. **Choose the commit identity** for the public history. If the Gmail address shouldn't be public:
   `git config user.email <id>+<user>@users.noreply.github.com` (GitHub → Settings → Emails).
3. **Build a fresh, history-free `main`** (keeps the working tree and the staged `git rm --cached`
   of `settings.json` and the `.pyc`; `git switch --orphan` would delete tracked files instead):

   ```sh
   git checkout --orphan main
   git add -A
   git add --chmod=+x packaging/install.sh packaging/uninstall.sh
   git ls-files -s packaging/install.sh packaging/uninstall.sh      # both must show 100755
   git status --short | grep -E 'settings\.json|pycache|CLAUDE\.local|\.cargo/|config\.json|KEEP|\.log$|handoff'   # must print nothing
   ```

4. **Verify the tree** that will be committed: `cargo build --release --workspace` and
   `cargo test --release --workspace` (warning-free), then the scan from `CONTRIBUTING.md` over the
   whole tree instead of a diff:

   ```sh
   git grep --cached -nE '([0-9]{1,3}\.){3}[0-9]{1,3}|([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}|[A-Za-z0-9+/]{32,}={0,2}|\+?[0-9][0-9 ()-]{8,}[0-9]' -- . ':!Cargo.lock'
   # plus your own private values, kept out of this file on purpose:
   git grep --cached -niE '<lan-prefix>|<windows-hostname>|<surname>|<email-user>|/home/<you>|/mnt/' -- .
   ```

   Expect only the placeholders and test fixtures listed above.
5. **Commit and push only `main`:**

   ```sh
   git commit -m "smartconnect-linux: Linux peer for the Motorola Smart Connect app"
   git log --oneline main           # exactly one commit, no parent
   git remote add origin <repository-url>
   git push -u origin main          # never --all or --mirror: the wip/ branch stays local
   git branch -D wip/checkpoint-2026-10-21      # optional, once main is pushed
   ```

6. **Fill in the repository URL** once it exists: `<repository-url>` in `README.md` and
   `docs/installation.md`, `repository` in the `Cargo.toml` files (six then, eight since files/ and fex/) and in
   `noctalia-plugin/smart-connect/manifest.json`, and `url=` in `packaging/arch/PKGBUILD`. Decide on
   the manifest's `author`. Consider the final app ID (`dev.smartconnect.Linux`, see
   `packaging-and-workspace.md`) before anyone installs the launcher.
7. **On GitHub:** enable private vulnerability reporting (`docs/security.md` sends reporters there).

Local cleanup, not part of the repository:

- Delete the stale per-crate target directories from before the workspace (they sit next to the
  workspace's target directory named in `CLAUDE.local.md`): `smartconnect-crosscontrol`,
  `-display`, `-display-clippy`, `-display-inputcheck`, `-gui` and `-webcam` (about 3.3 GB). Keep
  `smartconnect-mirror` (0.5 GB) until the Noctalia plugin's `mirrorBinary` setting is reset to
  `smartconnect-mirror`, then delete it too.
- `settings.json` untracking is done: commit `c2a22eb` removed it from the index (the file stays
  on disk, git-ignored), along with the `.pyc`. Both remain in the first WIP commit's history, which
  is why publishing uses an orphan commit.
- Optional code tidy-ups left out of this docs-only pass: point the code comments (15 files) that
  cite `evidence-2026-10-20/*-spec.md` at the matching `docs/protocol/` page, and drop
  `desktop/mirror_prototype.py` (the superseded Python viewer prototype).

## Why

- **A fresh orphan commit instead of rewriting history** (`git filter-repo`): there is one WIP
  commit and no remote, so nothing is lost, and an orphan commit can't leak a blob that a filter
  missed (the `.pyc` embeds a path, `settings.json` a developer path, older notes the hostname).
- **Code comments citing the private folder were not changed:** the pass was limited to docs, notes
  and privacy fixes, and those comments carry no private data, only a dangling file name. The index
  line explains the folder for readers of the notes.
- **The SMS test numbers were changed although they are common placeholders in India:** they are
  valid-looking mobile numbers that can belong to a real person, and the `555-01xx` range is
  reserved for fiction. The test exercises the same normalisation (spaces, dashes, a country code,
  last ten digits).
- **The handoff note was moved out of the checkout:** it holds machine-specific state (the owner's
  morning checklist), so it lives next to the repository instead of in `.notes/`, and the index no
  longer lists it.
- **The manifest author and the commit identity are left to the owner:** they are attribution
  choices, not leaks, and the LICENSE already names the owner.
