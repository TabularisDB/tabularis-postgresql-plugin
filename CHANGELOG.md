# Changelog

## [1.0.0-beta.2] - 2026-08-11

### Fixed

- Keyless-table numeric/temporal `WHERE` binding (port of `tabularis#618`):
  updating or deleting a row in a table with no primary key failed with
  `operator does not exist: numeric = text` (SQLSTATE 42883) whenever the
  identifying column was numeric or temporal, since `bind_pk_value` bound
  those JSON-string values as plain `TEXT` with no coercion. Factored the
  numeric/temporal coercion already used for `SET` binding
  (`bind_pg_string`) into two shared functions and routed `bind_pk_value`
  through them too. See PR #4 for the full TDD trail and live-database
  before/after verification.

## [1.0.0-beta.1] - 2026-08-11

### Changed

- Version scheme reset from `0.1.0` to `1.0.0-beta.1`. The actual target
  is `1.0.0` — Phase 1 byte-for-byte parity is complete per `tabularis`
  PR #577's own "CP-4 Gate Met" status — not an independent `0.x`
  development line, so SemVer's own prerelease mechanism
  (`1.0.0-beta.1` → `-beta.2` → `-rc.1` → `1.0.0`) expresses that
  relationship natively, which a bare `0.1.0` cannot.

## [Unreleased]

### Added

- `ci.yml`: a `version-suggestion` job posts a PR comment suggesting the
  next tag/version based on the PR title's Conventional Commits type
  (`feat`→minor, `fix`/`refactor`/`perf`→patch, `docs`/`style`/`chore`/
  `test`/`ci`/`build`→no release, `!`/`BREAKING CHANGE:`→major) and a
  required `prerelease:alpha|beta|rc|stable` PR label (missing label fails
  the job — no default channel is guessed). Purely informational, a
  precursor to eventually automating tag/release creation: nothing is
  tagged or released by this job. While the resolved baseline version
  carries a prerelease suffix matching the label's channel, the suggestion
  just increments that stage's counter (`1.0.0-beta.1` → `-beta.2`) rather
  than computing a full patch/minor/major bump — there's no shipped stable
  version yet to protect a SemVer contract against. Re-comments only when
  the PR's *derived classification* changes (type + breaking + channel),
  not on every title edit — tracked via a hidden marker in the comment
  body — and marks the previous suggestion as outdated via GitHub's
  `minimizeComment` API (same as the web UI's "Hide comment → Outdated")
  before posting the new one. Also widens the shared `pull_request:`
  trigger's `types:` to include `edited`/`labeled`/`unlabeled` (previously
  defaulted to `opened`/`synchronize`/`reopened` only, so a title-only edit
  never even re-ran CI). Checked precedent first: no sibling Tabularis
  plugin repo or `tabularis` itself has anything like this; a separate
  internal repo has a fuller PR-title-driven auto-tag/auto-release
  pipeline, but porting that whole pipeline was judged too large a
  behavioral change for this pass.
- Two more CI checks:
  - `release.yml`: a `validate` job gates the build matrix on the pushed
    tag matching `.tabularium`'s `version` field (stripped of the `v`
    prefix) — ported from the `tabularis-elasticsearch-plugin` sibling's
    pattern. This isn't just convention: the registry's own manifest
    schema documents this as a hard rule ("the registry rejects ingests
    whose tag and manifest version disagree"), so this catches the
    mismatch at tag-push time instead of at registry-submission time.
  - `ci.yml`: a `pr-title` job enforces Conventional Commits PR titles via
    `amannn/action-semantic-pull-request`, triggered on plain
    `pull_request` (not `pull_request_target`) since this repo doesn't
    need fork-PR support and the plain event avoids the elevated-
    permission surface entirely.
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

### Fixed

- `execute_query` returned `null` for every PostgreSQL enum column value,
  even when the database genuinely held a non-null value (issue #7).
  `extract.rs`'s `extract_value()` had no explicit match arm for enum
  types (custom-OID types), so they fell into the generic catch-all,
  which tries `row.try_get::<_, String>()` — `tokio_postgres`'s `FromSql
  for String` enforces known-OID checks and can't decode an arbitrary
  enum OID, so this always errored and silently nulled out, with no
  secondary fallback (unlike `try_extract<T>`'s string retry used
  elsewhere in the same file). Pre-existing since Sprint 5; the 82-test
  parity suite never caught it because it only exercises enum *writes*
  (`insert_record`/`update_record` binding) and enum *metadata*
  (`get_columns`'s `pg_enum` lookup), nothing reads an enum value back
  through `execute_query`. Fixed by decoding the raw label bytes directly
  as UTF-8 on `Kind::Enum` columns, ported from the builtin driver's
  `extract/enum.rs::extract_or_null` — bypasses `FromSql`'s OID
  enforcement entirely rather than working around it. Genuinely-NULL enum
  columns still correctly return `null`. Added unit tests for the new
  `EnumLabel` decode path plus a `tests/live_db.rs` regression test
  covering the full RPC round-trip.
- Flaky unit test: `get_or_create_pool_reuses_cached_entry_for_identical_params`
  and `cleanup_idle_pools_evicts_pools_with_no_checked_out_connections`
  both read/write the shared process-wide `POOLS` cache, and Rust's test
  harness runs `#[tokio::test]` fns concurrently — `cleanup_idle_pools`'s
  sweep (which iterates every cached pool, not just its own key) could
  evict the other test's freshly-inserted, still-idle pool mid-assertion.
  Reproduced locally at roughly 1-in-100 runs; hit for real in CI on the
  first push after two other CI fixes made this job the last one standing
  between failure and green. Serialized both tests behind a dedicated
  `tokio::sync::Mutex` (an async mutex, since the guard must span
  `.await` points — a `std::sync::Mutex` guard held across `.await`
  fails `clippy::await_holding_lock`).
- `Security audit` CI job was failing on every run — including, unnoticed,
  the very first `v1.0.0-beta.1`/`v1.0.0-beta.2` release builds — with
  `Resource not accessible by integration` when `rustsec/audit-check`
  tried to publish its Check Run result. The audit itself was passing
  (0 vulnerabilities found, our `RUSTSEC-2026-0235` ignore working
  correctly); the job had no `permissions:` block at all, so it inherited
  read-only default permissions instead of the `checks: write` the action
  needs. Added the missing `permissions:` block.
- `release.yml` never set `prerelease` on published GitHub releases —
  `softprops/action-gh-release` defaults to `false`, so both
  `v1.0.0-beta.1` and `v1.0.0-beta.2` published as (and one incorrectly
  displayed as "Latest") full stable releases despite being betas. Added
  tag-based auto-detection (`-` suffix ⇒ prerelease), matching
  `tabularis`'s own `release.yml` convention, plus `make_latest` wired to
  the same check so only an actual stable tag can become "Latest".
  Retroactively corrected both already-published releases via
  `gh release edit --prerelease`.
- Three of the new CI jobs above failed on their first real run and were
  fixed:
  - `Test` job: the existing `cargo test` (no target filter) tried to run
    `tests/live_db.rs` too, which panics immediately without a running
    PostgreSQL instance. Scoped to `cargo test --lib --bins`, leaving the
    live-DB test to its own dedicated `live-db-integration` job.
  - `Markdown lint` job: `docs/planning/.markdownlint.json`'s scoped
    override (`MD024`/`MD060`) only applied when markdownlint was invoked
    from within that directory — the CI step's root-level `**/*.md` glob
    never picked it up. Merged the scoped overrides into the single root
    `.markdownlint.json` instead of maintaining two config files. Also
    added `.markdownlintignore` (`target/`) since the glob was
    incidentally linting vendored third-party docs copied into build
    output by a dependency's build script.
  - `Security audit` job: `cargo audit` correctly found a real advisory,
    RUSTSEC-2026-0235 (vulnerable `rkyv` 0.7.46) — but it's pulled in only
    because `rust_decimal` lists it as an optional dependency behind a
    feature (`rkyv`) we never enable; confirmed no `rkyv` symbols are
    linked into the release binary. Added a documented `ignore:` entry for
    that specific advisory ID, since `cargo audit` scans the full
    `Cargo.lock` graph regardless of which optional features are active.

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
