# Contributing to smartconnect-linux

Thanks for your interest! `smartconnect-linux` is an unofficial, community-driven project providing a Linux peer implementation for the Motorola Smart Connect phone application. Bug reports, testing across diverse phone models and desktop environments, code improvements, and documentation contributions are all welcome.

Before getting started, please review the [README](README.md), [Architecture Guide](docs/architecture.md), and [Development Guide](docs/development.md). For larger architectural changes or new feature proposals, please open an issue first to discuss the design and approach.

---

## Development Workflow & Verification

The project is structured as a single Cargo workspace containing the daemon (`smartconnectd`), shared library, and helper crates (`mirror`, `crosscontrol`, `display`, `webcam`, `files`, `fex`, `gui`).

### Building
```bash
cargo build --workspace
# Or for release mode:
cargo build --release --workspace
```

### Running Tests
All unit and integration tests must pass cleanly:
```bash
cargo test --workspace
```

### Code Formatting
Ensure formatting complies with standard Rust rules:
```bash
# Check formatting
cargo fmt --check

# Automatically apply formatting fixes
cargo fmt
```

### Linting & Clippy
All workspace crates and targets must build with zero clippy warnings:
```bash
cargo clippy --workspace --all-targets -- -D warnings
```

---

## Branching & Pull Request Guidelines

1. **Branching Strategy:**
   - Create a topic branch from `master` (e.g. `feat/clipboard-filter`, `fix/display-touch-lag`, `docs/readme-update`).
   - Keep commits small, atomic, and focused. Write descriptive commit messages (Conventional Commits format is preferred, e.g. `feat(display): ...`, `fix(mirror): ...`, `docs: ...`).
2. **Pull Requests:**
   - Submit your pull request targeting the `master` branch.
   - Fill out all sections of `.github/PULL_REQUEST_TEMPLATE.md`.
   - Ensure the automated CI workflow passes (formatting, clippy, unit tests, cargo-deny, and install smoke tests).

---

## Clean-Room Engineering & Protocol Documentation

This project achieves interoperability with the Motorola Smart Connect ecosystem purely through clean-room network and protocol analysis under legal reverse-engineering exceptions for software interoperability.

- **Never copy decompiled, disassembled, or proprietary source code** into this repository, issues, or pull request discussions.
- Describe wire structures, byte offsets, message sequences, and behavior in your own words.
- When adding or clarifying protocol facts in `docs/protocol/`, clearly tag items as **OBSERVED** (empirically confirmed on the wire or in runtime traffic) or **INFERRED** (deduced from behavior or partial observation).
- Do not commit proprietary binaries, firmware, or extracted assets.

---

## Privacy & Redaction Rules

This repository and its issue tracker are fully public. **Never commit or submit real personal data or credentials:**

- **No Secrets or Identifiers:** Pairing keys, tokens, session passwords, RSA private keys, certificate fingerprints, device serial numbers, real phone numbers, contact names, SMS text, or Wi-Fi PSKs.
- **Placeholder Usage:** Always substitute test values or sanitized placeholders (e.g. `<device-id>`, `<token>`, `192.168.1.20`, `+15555550123`).
- **No Raw Captures:** Do not commit `.pcap`, `.pcapng`, `.etl`, `.apk`, or raw session logs without thorough verification and redaction.

Before submitting changes, inspect your git diff:
```bash
git diff origin/master... | grep -nE '([0-9]{1,3}\.){3}[0-9]{1,3}|([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}|[A-Za-z0-9+/]{32,}={0,2}|\+?[0-9][0-9 ()-]{8,}[0-9]'
```

---

## Pull Request Checklist

Before opening your pull request, verify that:
- [ ] `cargo fmt --check` succeeds.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` succeeds with no warnings.
- [ ] `cargo test --workspace` passes all tests.
- [ ] Any user-facing changes are documented in `docs/` and noted in `CHANGELOG.md` under `## [Unreleased]`.
- [ ] Any protocol additions are documented in `docs/protocol/`.
- [ ] No private data, real IP/MAC addresses, credentials, or proprietary assets are present in the diff.

---

## Reporting Security Vulnerabilities

Please do not open public issues for security vulnerabilities. Consult [SECURITY.md](SECURITY.md) for instructions on submitting private vulnerability reports via GitHub Security Advisories.

---

## License

By contributing to `smartconnect-linux`, you agree that your contributions will be dual-licensed under the terms of both the **MIT License** ([LICENSE-MIT](LICENSE-MIT)) and the **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE)).
