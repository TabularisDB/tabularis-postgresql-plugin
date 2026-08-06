# CLAUDE.md

## Status

This repo is intended to become the **primary home** for the PostgreSQL
plugin, pending sign-off. Once that happens, `TabularisDB/tabularis` PR #577
pivots from building the plugin in-tree to removing the built-in driver —
see `docs/planning/04-phase-3-deprecate-builtin.md`. For now, leave
`tabularis`'s `plugins/postgres-plugin/` untouched; this repo only receives
additions, nothing is removed from there.

The plugin source has landed here as a copy of the in-tree implementation
at commit `ad765f3a` (Phase 1 byte-for-byte parity proven: 82/82 parity,
72/72 baseline, 26/26 golden tests, 72 plugin unit tests — see
`docs/planning/02-phase-1-plugin-build.md` and
[`tabularis` PR #577](https://github.com/TabularisDB/tabularis/pull/577)).
Sign-off to promote this repo to primary is still pending several items
from that PR's own checklist: cross-platform build verification (only
macOS ARM confirmed so far), the 24-item manual smoke test, a security
audit pass, and a frontend regression check (`pnpm test` in `tabularis`).

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
  crud, ddl, blob).
- PostgreSQL access goes through `src/client.rs` (deadpool-postgres pool,
  cached by `host:port:database:user`).
- Value binding for INSERT/UPDATE lives in `src/binding.rs` — a strict
  ordered cascade (DEFAULT sentinel, BLOB wire format, boolean, numeric,
  temporal, enum CAST, UUID shape, PG array literal, TEXT fallback). Order
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
- **Testing**: prefer real PostgreSQL over mocks for integration-level
  behavior. Extract pure logic (SQL builders, value binding, pagination
  math) into testable functions with unit tests in a sibling `_tests.rs`
  file.
