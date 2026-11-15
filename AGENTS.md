# Notes for coding agents

## Installing token-reasoner (lge) for a user

Follow "Instructions for agents" in README.md, section 1. In short, on macOS:

```bash
scripts/install-macos.sh --connect-codex
```

Do not improvise other install steps, do not add `--autostart` or `--force` unless the
user asked, and report the page URL `http://127.0.0.1:8787` and the undo command
(`scripts/install-macos.sh --uninstall`) when done.

## Working on this repository

- Read `docs/ARCHITECTURE.md` first; its "Rules that never bend" apply to every change:
  LadybugDB is the only database, only `crates/lge-store` touches it, and every graph edge
  keeps its provenance (`static`, `inferred`, `unresolved`). A guess is never stored as
  `static`.
- Before you commit: `cargo test --workspace` and
  `cargo clippy --workspace --all-targets -- -D warnings` must pass.
