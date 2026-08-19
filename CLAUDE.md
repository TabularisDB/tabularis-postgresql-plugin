# CLAUDE.md

## Status

This repo is the **primary home** for the PostgreSQL plugin, published to
the Tabularium registry. It requires Tabularis v0.20.0 or later (the
release that introduced the plugin runtime this repo depends on) — see
`.tabularium`'s `min_runtime_version` field and the README's version
requirement note.

## Build & Test

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo test               # Run all tests
cargo clippy --all-targets -- -D warnings  # Lint
cargo fmt --all          # Format
```

## Cross-Repo Parity Check

This repo has no live-database parity suite of its own — the 82-test
byte-for-byte comparison against the built-in driver lives in
`tabularis`'s `src-tauri/tests/postgres_integration/parity*.rs` and is not
duplicated here (see `docs/planning/02-phase-1-plugin-build.md`'s "Repo
Extraction" section for the open question on where those tests should live
long-term). That suite resolves the plugin binary purely through the
`POSTGRES_PLUGIN_BIN` env var, so it can validate this repo's binary with
zero changes on the `tabularis` side. Re-run this any time this repo's
source diverges from the in-tree copy, to catch parity drift immediately:

```bash
# 1. Build the release binary from this repo
cargo build --release
STANDALONE_BIN="$PWD/target/release/postgresql-plugin"

# 2. Point tabularis's existing (unmodified) parity suite at it
cd /path/to/tabularis
bash tests/fixtures/seed_postgres.sh
POSTGRES_PLUGIN_BIN="$STANDALONE_BIN" RUST_TEST_THREADS=1 \
  cargo test --manifest-path src-tauri/Cargo.toml --test postgres_integration parity -- --include-ignored
```

Expected result: the same 82/82 that the in-tree binary produces. Any test
going RED here means the extraction changed behavior and must be fixed
before merging — the same red→green discipline used throughout the
migration, now applied across the repo boundary.

## Architecture Rules

- All RPC handlers are async and return `serde_json::Value`.
- Handlers live in `src/handlers/`, organized by domain (connection, metadata,
  crud, ddl, blob, query).
- PostgreSQL access goes through `src/client.rs` (deadpool-postgres pool,
  cached by `host:port:database:user:startup_script`).
- Value binding for INSERT/UPDATE lives in `src/binding.rs` — a strict
  ordered cascade (DEFAULT sentinel, BLOB wire format, enum CAST, boolean,
  numeric, temporal, UUID shape, PG array literal, TEXT fallback). Order
  matters; do not reorder without understanding why each earlier step must
  run first.
- Never write to `stdout` outside the JSON-RPC response loop in `main.rs` —
  any stray write corrupts the protocol stream. Use `log`/stderr for
  diagnostics.

## Key Patterns

- **Adding a new RPC method**: add to `rpc.rs`'s dispatch table, implement in
  the appropriate `handlers/` module.
- **Parity with the built-in driver**: this plugin exists to replace a
  built-in PostgreSQL driver, byte-for-byte. When porting a method, read the
  built-in's implementation first and match its SQL/behavior exactly —
  don't improve on it silently; behavioral differences are regressions here,
  not fixes.
- **Parity gaps beyond SQL text**: the 82-test parity suite (see "Cross-Repo
  Parity Check" above) and this repo's own SQL-builder unit tests cover
  query text well, but four gaps found by a full source-level audit against
  the builtin (`src-tauri/src/pool_manager.rs` and
  `src-tauri/src/drivers/postgres/`) all lived in code paths that suite
  doesn't exercise directly: TLS/pool-config semantics (issues #34, #36,
  #38 — mTLS client-cert not honored, pool cache key ignoring TLS params
  entirely, `verify-ca` incorrectly enforcing hostname checks) and
  wire-format type coverage in `extract.rs` (#39 — `MONEY` silently
  decoding to `null`). When auditing for parity, read the builtin's actual
  Rust source for the subsystem (not just its SQL strings) — connection/TLS
  config and `extract.rs`'s `Type::` dispatch table are the areas most
  likely to silently diverge, since they're exercised by config values and
  column types rather than by query shape.
- **TDD for parity bugs without a live database**: prove the divergence
  with a standalone unit test *before* fixing it, even when the real bug
  only manifests during a live TLS handshake or a live column read. For
  TLS/verifier bugs, construct the verifier type directly and call
  `verify_server_cert` against a real (but locally-generated, long-validity)
  cert/CA fixture — no live server needed (see `#38`'s
  `verify_ca_cert_verifier_accepts_a_chain_valid_cert_with_mismatched_hostname`
  test in `src/client_tests.rs`). For wire-format bugs, call the type's
  `FromSql::from_sql`/`accepts` directly with hand-built wire bytes (see
  `#39`'s `money_decodes_the_same_8_byte_wire_format_as_int8` in
  `src/extract_tests.rs`). Confirm the test fails against the current code
  for the right reason before implementing the fix.
- **Testing**: prefer real PostgreSQL over mocks for integration-level
  behavior. Extract pure logic (SQL builders, value binding, pagination
  math) into testable functions with unit tests in a sibling `_tests.rs`
  file.
- **PR titles and versioning**: PR titles must be Conventional Commits
  (`type: subject`) and every PR needs a `prerelease:alpha|beta|rc|stable`
  label — both enforced by CI. See the README's "Contributing: PR Titles &
  Versioning" section for the full convention and the type→bump mapping.
  CI posts (and keeps up to date) a version-suggestion comment based on
  these — informational only, nothing is tagged/released automatically yet.
