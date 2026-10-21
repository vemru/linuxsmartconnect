# Contributing to smartconnect-linux

Thanks for your interest! smartconnect-linux is an unofficial, community project that lets the stock
Motorola Smart Connect app work with Linux. Bug reports, testing on other phones and desktops, code
and documentation are all welcome.

Before you start, read the [README](README.md), [Architecture](docs/architecture.md) and
[Development](docs/development.md). For anything bigger than a small fix, open an issue first so we
can agree on the approach.

## Ground rules

### Documentation ships with the code

A change isn't done until its documentation says how to use it, what has been verified and what
hasn't. Update everything the change affects, in the same pull request:

| Where | What goes there |
|---|---|
| `.notes/<feature>.md` | Design log: **what** changed, **how** (approach, key files and functions) and **why** (reasoning, alternatives, constraints). Add a Mermaid diagram when a flow or state machine is easier to see than to read. Add one line per note to `.notes/index.md`. |
| `docs/` | User and developer docs: installation, usage, troubleshooting, architecture, [control socket API](docs/control-socket-api.md), development, security. |
| `docs/protocol/` | The wire protocol per service: commands, payload shapes, sequences, gotchas. Tag every fact **OBSERVED** (seen on the wire or in code) or **INFERRED**. |
| `README.md` | The feature matrix (status of each feature) and the quick start. |
| `CHANGELOG.md` | One line per user-visible change under `## [Unreleased]`. |

Use the status keys consistently: ✅ verified on a real phone · 🟡 implemented, not verified on a
phone yet · ❌ not supported (and 🚧 for work in progress that isn't usable yet). Split a feature
into rows when only part of it is verified. Only mark something ✅ when you have seen it work on a
phone, and say which phone and Smart Connect version in the PR.

### Privacy rules

The repository is public. Never commit:

- **Raw research material:** packet captures, logcats, ETL traces, memory dumps,
  decompiler/disassembler output, Motorola APKs or Windows binaries, Smart Connect `config.json`
  files.
- **Real secrets or personal data:** pairing keys, tokens, certificate fingerprints, device ids and
  serials, MAC or IP addresses, phone numbers, contact names, notification or SMS text, Wi-Fi network
  names or passwords. Use placeholders such as `<token>`, `<device-id>`, `<key>`, `192.168.1.20`,
  `+15555550123`.
- **Session logs** (`~/.local/share/smartconnect-linux/sessions/`) or excerpts of them, unless every
  value has been replaced by a placeholder.
- **Screenshots** that show real notifications, contacts, messages, wallpapers or network names.

Before you push, scan your diff and look at every hit:

```sh
git diff origin/main... | grep -nE '([0-9]{1,3}\.){3}[0-9]{1,3}|([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}|[A-Za-z0-9+/]{32,}={0,2}|\+?[0-9][0-9 ()-]{8,}[0-9]'
```

That pattern finds IPv4 addresses, MAC addresses, long base64 strings (keys, tokens, ids) and
phone-number-like strings. Placeholders such as `192.168.1.20` will match too; that's fine.

### Clean-room work

This project works with Smart Connect through protocol analysis for interoperability. **Never copy
decompiled or disassembled code** into the repository, not even as a comment. Describe behaviour in
your own words. Naming a class or method is fine when it helps a reader find something. No Motorola
code, binaries or assets belong here.

### Code

- The repository is one Cargo workspace: the daemon at the root, plus `mirror/`, `crosscontrol/`,
  `display/`, `webcam/`, `files/`, `fex/` and `gui/`. Before you open a pull request, run:

  ```sh
  cargo build --release --workspace
  cargo test --release --workspace
  ```

  Builds must be **warning-free**. While working on one crate, `-p <package>` is enough.
- Programs find each other through `SMARTCONNECT_<NAME>_BIN`, their own directory and `PATH`
  (see [Installation](docs/installation.md#how-the-programs-find-each-other)). Never hard-code a
  path into your home or build directory.
- Match the style of the surrounding code. Comments should explain *why*, especially when a value
  or shape exists because the phone requires it.
- Keep `smartconnectd` free of GTK, GStreamer and Wayland dependencies. Media-heavy features belong
  in helper processes that use the [control socket](docs/control-socket-api.md).
- Advertise a service to the phone (`src/proto.rs`) only once its PC side works. The phone draws
  tiles for advertised services, and a tile with nothing behind it fails in confusing ways.
- See [Adding a service](docs/development.md#adding-a-service) for the full checklist.

## Pull request checklist

- [ ] `cargo build --release --workspace` is warning-free and `cargo test --release --workspace`
      passes
- [ ] Tested on a phone (say which model, Smart Connect version, distribution and compositor), or
      the feature is marked 🟡 in the docs
- [ ] `.notes/<feature>.md` written or updated, and `.notes/index.md` in sync
- [ ] Affected `docs/` pages updated (usage, troubleshooting, control socket API, …)
- [ ] Protocol facts added to `docs/protocol/<service>.md`, tagged OBSERVED or INFERRED
- [ ] README feature matrix updated if a feature's status changed
- [ ] One `CHANGELOG.md` line per user-visible change under `## [Unreleased]`
- [ ] Privacy scan of the diff done; only placeholders remain

## Reporting bugs

Open an issue with:

- your phone model and Smart Connect version (Android *Settings → Apps → Smart Connect*);
- your distribution, compositor or desktop, and how you run the daemon (systemd unit or by hand);
- the version (`smartconnectd --version`) or commit;
- what you did, what you expected, and what happened;
- the relevant part of `journalctl --user -u smartconnectd`, and helper logs if they are involved
  (see [Where the logs are](docs/troubleshooting.md#where-the-logs-are)).

**Remove personal data from logs before posting:** IP and MAC addresses, ids, keys, tokens, names,
phone numbers, message text. Don't attach session logs or phone logcats unless you have redacted
them completely.

Security problems: please report them privately as described in
[docs/security.md](docs/security.md#reporting-a-vulnerability), not in a public issue.

## Coding agents

[`CLAUDE.md`](CLAUDE.md) restates these rules for AI coding agents working in this repository. Keep
it in sync when the rules change. Machine-specific notes belong in the git-ignored
`CLAUDE.local.md`.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).
