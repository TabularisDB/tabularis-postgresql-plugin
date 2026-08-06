# Changelog

## [Unreleased]

### Added

- CI hardening — deliberately set a higher bar than the sibling plugin
  repos and the org's own documented requirements (no sibling runs
  `cargo audit`; only 2 of 11 Rust siblings gate on clippy/fmt at all):
  - `.tabularium` manifest validation against the live registry schema via
    `@tabularium/cli validate`, catching a malformed manifest automatically
    (we got `name`/`id` wrong once by hand earlier this session).
  - `markdownlint-cli` as an enforced CI job, not a manually-run habit.
  - A release-binary smoke test: pipe a trivial `initialize` JSON-RPC
    request into each freshly-built platform binary and assert a valid
    (non-error) response before it ships in a zip. Skipped for `linux-arm64`
    only, since that leg is cross-compiled and the binary can't execute on
    the x86_64 build runner without QEMU emulation.
  - `cargo audit` (via `rustsec/audit-check`) for supply-chain
    vulnerabilities, on every push/PR and a weekly schedule (catches CVEs
    disclosed after merge against unchanged dependencies).
  - `tests/live_db.rs`: a self-contained live-`postgres:16`-container
    integration test (first top-level `tests/` dir in this repo — existing
    tests are all pure unit tests via the `.rules/rust.md` #4/#5
    sibling-file convention). Covers connect, a basic query, an insert, and
    the `startup_script`/`connection_string` handlers found completely
    uncovered during the security-audit pass — closes the actual biggest
    gap in this repo's CI: nothing previously verified the binary against
    a real database automatically. Deliberately NOT the cross-repo 82-test
    parity suite (that stays a manual/periodic check against `tabularis`,
    per the "Repo Extraction" open question in
    `docs/planning/02-phase-1-plugin-build.md`).
  - Two further hardening ideas — `dependency-review-action` on PRs and
    SBOM generation via `cargo-cyclonedx` — were considered and
    deliberately deferred rather than implemented now; see
    `docs/planning/ci-hardening-deferred.md` for the rationale.
- `main.rs` rewritten to a worker-pool architecture (4 workers + a single
  writer task + a dedicated pool-cleanup task, coordinated via a
  `tokio::sync::watch` shutdown signal on stdin EOF), matching the
  sqlserver/dynamodb sibling plugins. A slow query on one connection no
  longer blocks a concurrent `ping` or metadata call on another; the host
  already tolerates out-of-order responses (it correlates by JSON-RPC `id`
  via a `HashMap`, not arrival order), so this required no protocol change.
- Periodic idle-pool eviction: every 10 minutes, `client::cleanup_idle_pools()`
  drops cached connection pools that currently have no checked-out
  connections, so a long-running session that has connected to many
  distinct targets doesn't pin idle TCP connections and pool memory for
  the plugin's lifetime. Matches the sqlserver/dynamodb sibling plugins'
  pattern exactly (`pool.status().size > pool.status().available` as the
  keep predicate). Found missing during the same security-audit pass that
  flagged "pool cleanup on shutdown" — investigation showed the host never
  sends the plugin a `shutdown` RPC call at all (it kills the process
  outright), so that specific checklist wording described something
  unreachable; comparing sibling plugins surfaced this as the real,
  exercisable gap instead. Added test-first (TDD): a unit test asserting
  an idle pool gets evicted, written and confirmed RED (`cleanup_idle_pools`
  didn't exist) before the function was implemented to GREEN.
- `save_blob_to_file` now validates `file_path` (empty, existing-directory,
  or missing-parent-directory) before spending a DB round-trip on a write
  that would fail anyway — a clearly attributed `-32602` error instead of
  a bare OS error number surfacing after the query already ran. Not a
  security boundary (the path comes from the frontend's native save
  dialog), just a fast-fail. The builtin driver's identical gap is
  untouched; this fix is plugin-only. Found during the security-audit pass.
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
