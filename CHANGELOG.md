# Changelog

## [Unreleased]

### Fixed

- Client certificate authentication (mTLS) for PostgreSQL servers requiring
  it (e.g. Google Cloud SQL). `ConnectionParams` already carried `ssl_cert`
  and `ssl_key`, but `build_tls_connector` never read them — every TLS
  branch called `.with_no_client_auth()` unconditionally, so connections
  failed with "connection requires a valid client certificate" the same way
  the builtin driver's `pool_manager.rs` did before it was fixed upstream
  (TabularisDB/tabularis#666). Added `load_client_cert_from_pem`, reusing
  the same `rustls::pki_types::pem::PemObject` machinery as the existing
  `load_roots_from_pem` (rather than reintroducing `rustls-pemfile`, removed
  above for being unmaintained) — `PrivateKeyDer` supports PKCS1/SEC1/PKCS8
  via the same trait. Both TLS branches now present the client cert via
  `.with_client_auth_cert(...)` when `ssl_cert`/`ssl_key` are set, and
  `build_tls_connector` errors clearly if only one of the pair is provided.
- Pool cache key ignored every TLS param (`ssl_mode`/`ssl_ca`/`ssl_cert`/
  `ssl_key`) — `connection_key` matched only on
  `host:port:database:user:startup_script`, so two connections to the same
  target differing only in TLS configuration (e.g. `require` vs.
  `verify-full`, or different client certs) could incorrectly share a
  cached pool and its already-negotiated TLS setup. This wasn't inherited
  from the builtin driver: the builtin's `build_connection_key` already
  keyed on `ssl_mode`/`ssl_ca` before this plugin's `client.rs` was even
  staged, so this was a parity miss during extraction, not a later
  upstream change. Folded all four TLS params into `connection_key`,
  matching the builtin's TLS-param keying.
- `ssl_mode=verify-ca` incorrectly enforced hostname verification —
  `build_tls_connector` wrapped `rustls::client::WebPkiServerVerifier` for
  `verify-ca`, whose `verify_server_cert` unconditionally checks the
  hostname with no way to opt out, making `verify-ca` behave identically to
  `verify-full`. `verify-ca` is supposed to validate the certificate chain
  but skip hostname verification — that's the entire distinction from
  `verify-full` (matches libpq `sslmode=verify-ca` semantics). Added a
  dedicated `VerifyCaCertVerifier`, ported from the builtin driver's
  `src-tauri/src/pool_manager.rs`, using
  `rustls::client::verify_server_cert_signed_by_trust_anchor` directly
  instead. Proved the bug and the fix with a chain-valid cert whose
  hostname deliberately doesn't match the connection target: rejected
  before this fix, accepted after (while a CA-untrusted cert is still
  correctly rejected, and `verify-full` still correctly rejects the
  hostname mismatch).
- MONEY columns always read as `null` — `extract_value` had no case for
  `Type::MONEY`, so it fell through to the generic string fallback, but
  `String`'s `FromSql::accepts` returns `false` for `Type::MONEY` (confirmed
  directly), making that fallback fail every time regardless of the actual
  value. MONEY is listed as a supported numeric type in the README and
  `ddl.rs`'s implicit-cast-compatible group, but reading it back was
  silently broken. Added a `Money` wrapper (`src/extract.rs`) that decodes
  the same 8-byte big-endian i64 wire format `INT8` uses and reuses the
  existing `i64_to_json` JS-safe-integer stringification, matching the
  builtin driver's `extract/advanced_types.rs::Money`.

## [1.0.0-beta.7] - 2026-08-17

### Removed

- `rustls-pemfile` dependency. It's unmaintained (RUSTSEC-2025-0134 —
  archived upstream, no CVE) and was our only source of the
  `Security audit` job's `unmaintained` warning below. Its one call site
  (`load_roots_from_pem` in `client.rs`, parsing a user-supplied `ssl_ca`
  bundle for `verify-ca`/`verify-full` connections) migrated to
  `rustls::pki_types::CertificateDer::pem_slice_iter`, the maintained
  replacement the advisory itself recommends — already in our dependency
  tree transitively via `rustls`, so this is a dependency removal, not an
  addition. Verified against a disposable self-signed-cert Postgres
  container (`verify-ca` connects correctly; an invalid PEM file still
  produces the same clear error) and added 3 unit tests for
  `load_roots_from_pem` directly, which had zero coverage before this.

### Fixed

- `Security audit` CI job failed on its first scheduled (cron) run with
  `Resource not accessible by integration` when `rustsec/audit-check`
  tried to file a tracking issue for an informational `unmaintained`
  warning (`rustls-pemfile`, RUSTSEC-2025-0134 — no CVE, just an
  archived-upstream notice). The job's `permissions:` block already had
  `checks: write` from a prior fix (CI publishing its pass/fail status),
  but not `issues: write` — a separate permission the action only
  exercises on cron-triggered runs when it has a warning to report, which
  is why push/PR runs of this same job never surfaced the gap. Added the
  missing `issues: write` permission.

## [1.0.0-beta.6] - 2026-08-14

### Fixed

- README's header logo/plus-icon and screenshot-gallery images used
  repo-relative `src` paths (`assets/plus.svg`, `assets/screenshots/*.png`).
  GitHub auto-resolves those against the repo's raw-content base, so they
  rendered fine there — but the Tabularium registry serves the README's
  HTML standalone, with no such resolution, so all 6 images 404'd on the
  plugin's registry page once the README actually synced. Switched to
  absolute `raw.githubusercontent.com` URLs, matching the pattern already
  used for `postgresql-icon.svg` and the `.tabularium` `screenshots` array.

### Changed

- `.tabularium`'s `description` rewritten from the internal-facing
  "PostgreSQL plugin driver for Tabularis (parity implementation)" to a
  capability-focused tagline (schemas/tables/views/routines/triggers,
  EXPLAIN plans, type-aware row editing, DDL generation) matching the
  style of the strongest sibling plugin descriptions on the Tabularium
  registry (`mongodb`, `firestore`). Synced the GitHub repo's own
  description field to match, since it was still the old wording.

## [1.0.0-beta.5] - 2026-08-14

### Added

- `.tabularium`: `min_runtime_version: "0.20.0"` — per debba's guidance,
  `tabularis` 0.20.0 is the first release expected to ship the #614/#577
  host-side fixes (capability-driven identifier quoting, etc.) this
  plugin depends on for correct behavior under a non-`"postgres"` driver
  id. Older runtimes will be refused rather than silently misbehaving.
- README header: a plus icon between the Tabularis and PostgreSQL logos
  (`assets/plus.svg`, a lucide-style glyph matching the icon set
  `tabularis`'s own frontend uses) to read as "Tabularis + PostgreSQL" at
  a glance. Self-hosted a copy of the PostgreSQL project's 3-colors logo
  (`assets/postgresql-logo-3colors.png`) instead of hotlinking
  `wiki.postgresql.org`, for the same reliability reason `postgresql-icon.svg`
  is already self-hosted rather than pointed at a third party.
- `.tabularium`: `screenshots` array (7 real captures, not mockups —
  fresh install, database picker, connection form, successful test,
  saved connection, multi-schema browser, and a live data grid showing a
  real enum value) plus a matching "Screenshots" section in the README,
  for the plugin's eventual Tabularium registry submission.
- `.tabularium`: registry-listing metadata fields — `category`, `tags`,
  `license`, `readme`, `homepage`, `documentation_url`, `support.issues_url`,
  `color` — needed for the plugin's eventual Tabularium registry submission.
  These are purely presentational for the registry's plugin-card/detail
  page; the builtin driver's own definition
  (`tabularis`'s `src/hooks/useDrivers.ts`) has none of them, confirming
  they carry no runtime behavior. No version bump warranted.

### Fixed

- README's release badge showed "no releases or repo not found" because
  every published release so far (`v1.0.0-beta.1` through `.4`) is flagged
  `prerelease: true`, and GitHub's `/releases/latest` API — which the
  unqualified `img.shields.io/github/release/...` badge queries — excludes
  prereleases by design. Switched to `img.shields.io/github/v/release/...
  ?include_prereleases`, confirmed rendering `v1.0.0-beta.4` correctly.
- README's installation section and work-in-progress banner were stale:
  described a hypothetical "Automatic (via Tabularis)" install path with
  no registry to install from yet, and the banner's sign-off checklist
  didn't reflect that `tabularis` PR #577's checklist is now fully checked
  (though the PR itself remains open/unmerged). Updated both to describe
  the actual current state — manual install only, registry submission
  pending — and added a "From the Tabularium registry" placeholder section
  matching the pattern used by already-published sibling plugins
  (`tabularis-elasticsearch-plugin`, `tabularis-duckdb-plugin`).

## [1.0.0-beta.4] - 2026-08-13

### Fixed

- Plugin had no `icon` declared in `.tabularium`, so every connection using
  it fell through to the generic fallback icon in the sidebar, connection
  list/cards, and the new-connection engine picker (`getDriverIcon`/
  `getConnectionIcon` in `tabularis`'s `src/utils/driverUI.tsx` only render
  the branded PostgreSQL mark for the literal built-in driver id
  `"postgres"`; anything else needs a manifest-declared `icon` URL).
  Added `postgresql-icon.svg` — the exact same Slonik elephant path/viewBox
  already used by the builtin driver's `PostgreSQLIcon` component
  (`tabularis`'s `src/utils/driverIcons.tsx`), extracted as a standalone
  file and colored to PostgreSQL's brand blue (`#336791`) — and pointed
  `.tabularium`'s `icon` field at its raw GitHub URL, matching the
  mongodb/dynamodb sibling plugins' convention. Verified against the live
  registry schema, which documents `icon` as a hosted image URL.

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

## [1.0.0-beta.3] - 2026-08-12

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
