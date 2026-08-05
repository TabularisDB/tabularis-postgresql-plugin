# CLAUDE.md

## Status

This repo currently holds only repo-level scaffolding (license, CI/release
workflow shape, contributor docs). The plugin source itself
(`Cargo.toml`, `src/`, `.tabularium`) has not been migrated yet — it still
lives at `TabularisDB/tabularis`'s `plugins/postgres-plugin/`. Do not assume
the CI workflow passes or that a `cargo build` will succeed until that
migration lands.

## Build & Test

Once the source has been migrated:

```bash
cargo build              # Debug build
cargo build --release    # Release build
cargo test               # Run all tests
cargo clippy --all-targets -- -D warnings  # Lint
cargo fmt --all          # Format
```

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
