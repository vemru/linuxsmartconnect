## Summary
<!-- Briefly explain what changes this PR introduces, why, and how. -->

## Checklist
Please verify before submitting:

- [ ] **Privacy & Redaction:** No real personal data, secrets, pairing keys, tokens, device IDs, serial numbers, real IP/MAC addresses, phone numbers, contact names, SMS text, or local home paths (`/home/...`) are included in code, tests, logs, or commit messages.
- [ ] **Documentation:** User-facing documentation in `docs/` and `README.md` has been updated if applicable.
- [ ] **Wire Protocol:** If Smart Connect wire protocol messages or behaviors were altered, `docs/protocol/` has been updated with facts marked **OBSERVED** or **INFERRED**.
- [ ] **Design Log:** For significant features or refactors, `.notes/<feature>.md` and `.notes/index.md` have been updated.
- [ ] **Changelog:** User-visible changes are noted in `CHANGELOG.md` under `## [Unreleased]`.
- [ ] **Build & Tests:**
  - `cargo fmt --check` passes cleanly.
  - `cargo clippy --workspace --all-targets -- -D warnings` produces zero warnings.
  - `cargo test --workspace` passes all tests.
