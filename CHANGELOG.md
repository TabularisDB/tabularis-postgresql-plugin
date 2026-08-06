# Changelog

## [Unreleased]

### Added

- Plugin source (`Cargo.toml`, `Cargo.lock`, `.tabularium`, `src/`) imported
  from `TabularisDB/tabularis`'s `plugins/postgres-plugin/` at commit
  `ad765f3a` (82/82 parity tests green per that commit). This is a parallel
  copy — the in-tree source has not been removed, and the two copies are
  kept in sync manually pending a later decision to deprecate the in-tree
  copy.
- `docs/planning/`: the 8 design documents that shaped this migration
  (phase docs, both migration-plan variants, and the feature-gap audit
  feeding Phase 2), copied from `tabularis`'s `.github/planning/`.
- `src/lib.rs` and `src/bin/test_plugin.rs`: extracted the plugin's module
  tree into a library crate so the justfile's `repl` recipe (a local
  JSON-RPC REPL) has a real binary to run, matching the oracle/dynamodb
  sibling plugins' structure.
- Repo scaffolding: `LICENSE` (Apache-2.0), `.gitignore`, `.editorconfig`,
  `CODEOWNERS`, `rust-toolchain.toml` (pinning `rustfmt`/`clippy`),
  `.github/dependabot.yml`, `justfile` (build/test/lint/fmt/dev-install/
  demo-db recipes, matching sibling plugin repos).
- `CI` workflow (`cargo build`/`test`/`clippy`/`fmt --check`) and `Release`
  workflow (cross-platform binary builds for Linux x86_64/aarch64, macOS
  x86_64/aarch64, Windows x86_64, published as GitHub release assets
  alongside `.tabularium`).
- `README.md` and `CLAUDE.md` describing the plugin's purpose, architecture,
  and current migration status.

### Changed

- `.tabularium`'s `name` field changed from `postgres-plugin` to
  `postgresql` (and the redundant `id` field dropped) to match this repo's
  own install-path/executable naming and the sibling-plugin convention of a
  bare engine-name slug. This field is a permanent registry slug once
  published, so it was fixed before any release.
