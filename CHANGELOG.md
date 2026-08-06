# Changelog

## [Unreleased]

### Added

- `startup_script` support: SQL supplied on the connection now runs on every
  new pooled connection via a `deadpool-postgres` `post_create` hook, with a
  preflight validation pass so a broken script fails fast with a clearly
  attributed `Startup script failed: ...` error instead of a misleading
  connection error. Matches the builtin driver's
  `run_postgres_startup_script` behavior (`src-tauri/src/pool_manager.rs`).
  Found missing during a security-audit pass — no parity test exercises
  this field, so the 82/82 parity suite didn't catch the gap.
- `connection_string` support: when present, it's parsed via
  `tokio_postgres::Config::from_str` and takes precedence over the discrete
  host/port/database/username/password fields, matching the README's
  documented behavior. Previously the field was parsed into
  `ConnectionParams` but silently never consumed by `build_pool()`. Also
  found during the security-audit pass.
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
- README's connection config table: `ssl_ca`/`ssl_cert`/`ssl_key` were
  documented as a single group ("If using `verify-ca`/`verify-full`"), but
  only `ssl_ca` (custom CA pinning) is actually implemented — matches the
  builtin PostgreSQL driver, which also has no client-certificate support
  (unlike its MySQL driver). Documentation corrected to describe only what
  the plugin (and builtin) actually do.
