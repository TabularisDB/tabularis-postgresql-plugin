# Changelog

## [Unreleased]

## [1.0.0-rc.13] - 2026-10-29

### Fixed

- A `prepare` failure inside an open transaction no longer cascades to a
  confusing 25P02 ("current transaction is aborted") on every subsequent
  statement in the same session (#170, #180). The pre-resolve `prepare` in
  `exec_query_on_client` swallowed its error via `.ok()` and ran
  `query_typed` unconditionally; a Parse-time error (e.g. `SELECT * FROM
  missing_table` → 42P01) inside an open transaction aborted the
  transaction server-side, so `query_typed` failed with 25P02 instead of the
  real error — and because `transaction_effect("SELECT")=None` kept the
  batch caller's `in_transaction=true`, the aborted connection was re-pinned
  and every subsequent batch 25P02-cascaded until an explicit `ROLLBACK`.
  The fix gates the swallow on `in_transaction`: inside an open transaction
  it short-circuits (best-effort ROLLBACK + surface the prepare's real
  SQLSTATE + signal `txn_aborted=true` so the caller forces
  `in_transaction=false` and releases the connection); outside a
  transaction the recoverable swallow is preserved (a 42P05 on a real
  PgBouncer bare batch where `query_typed`'s unnamed protocol can still
  succeed). The error message tells the user the transaction was rolled
  back so the state change isn't silent.

- A `query_typed` / `execute_typed` runtime error inside an open
  transaction no longer re-pins the aborted connection and 25P02-cascades
  the next statement (#181, #184). The sibling of #170 from a different
  source: a runtime error at execute time (division by zero, a constraint
  violation at fetch, a failed DDL/write, a 42P01 on a relation dropped
  between prepare and fetch) also aborts the transaction, but #170
  deliberately left those two error arms returning `txn_aborted: false`.
  The fix classifies on the error's `as_db_error()` (a server `DbError` is
  any ERROR-class response — WARNING/NOTICE never arrive as a query
  `Error`, they're delivered out-of-band as `AsyncMessage::Notice`, so
  every `DbError` aborts an open transaction); non-`DbError` errors
  (transport/timeout) are conservatively assumed to have aborted it. Both
  error arms now ROLLBACK + surface the real error + signal
  `txn_aborted=true`, gated on `transaction_effect(query)` being neither
  `Closes` nor `Chains` so a failed `COMMIT`/`ROLLBACK`/`END`/`COMMIT AND
  CHAIN` — which already ended the transaction server-side and is handled
  by the existing `Closes`/`Chains` path — does not get a spurious ROLLBACK
  and "rolled back" notice. Closes the 25P02 cascade end-to-end (#170
  prepare-failure + #181 runtime-error = the two faces).

### Dependencies

- `uuid` bumped from 1.28.0 to 1.29.0 (#182). Additive only — adds
  versioned constructors to `NonNilUuid`; the existing `Uuid` API the
  plugin uses (`parse`, `from_sql`, `to_string`) is untouched. Semver-
  compatible with the existing `uuid = "1.20"` constraint.

- `tokio` bumped from 1.53.3 to 1.54.1 (#183). The 1.54.1 patch reverts
  the only `time`-subsystem change from 1.54.0 (a timer regression), so
  `tokio::time::timeout` behavior is back to 1.53.3-equivalent — the
  bound the #170/#181 fixes rely on. All other 1.54.0 changes are
  additive or internal; none touch the APIs the plugin uses
  (`time::timeout`, `sync::Mutex`, `spawn`, `#[tokio::test]`). MSRV
  bumped to 1.85.

### Docs

- Centered the PostgreSQL banner and converted it to PNG; fixed a stale
  Tabularis logo URL (#179).

## [1.0.0-rc.12] - 2026-10-23

### Fixed

- Querying a table with a non-builtin PostgreSQL type (custom `enum`,
  composite, domain, range, or extension type like `citext` / Supabase
  `auth.uid`) no longer hangs on a **direct** connection (#166, #168) once
  the result is large enough to fill tokio-postgres's bounded response
  channel — the rc.11 fix only covered the transaction-pooler path. The
  direct path now pre-resolves result column types via a `prepare` on the
  shared cached client before the `query_typed` data fetch, so
  tokio-postgres's internal `typeinfo_statement` resolves in a clean window
  (not mid-stream) and the cache hit makes `get_type` return early. Same
  mechanism the `in_transaction` batch path uses; preserves pool reuse and
  has zero overhead on builtin types.

- A batch or interactive-transaction session on a transaction-mode pooler
  no longer deadlocks on a wide non-builtin-typed table (#169, #172) — the
  rc.11 #166 fix and the rc.10 #164 one-shot path both only covered
  non-session single result-set queries. `run_batch_in_session` now makes
  the same per-query routing decision as the single-query path: a
  result-set query inside an open transaction pre-resolves on the shared
  client (the user's `BEGIN` pins the backend); one with no open
  transaction routes to the one-shot pinned path. The empty-rows
  column-discovery fallback is guarded for the session-pinned pooler case.

- A bare batch (no `session_id`, no `BEGIN`) that sets session-local state
  in an earlier statement — a session `SET` (not `SET LOCAL`) or a
  `CREATE TEMP[ORARY] TABLE` — then runs a result-set `SELECT` no longer
  silently loses that state on a `pooler_mode = transaction` connection
  (#173, #174). The rc.11 #172 fix routed the `SELECT` to a fresh one-shot
  connection that never saw the `SET`/temp table (wrong precision, or
  `relation does not exist`). The fix detects the prior session state and
  keeps the `SELECT` on the shared batch client with a column-type
  pre-resolve, restoring `main`'s within-batch guarantee. Handles the
  optional deprecated `GLOBAL`/`LOCAL` qualifier before `TEMP`/`TEMPORARY`
  and excludes `SET LOCAL` / `SET TRANSACTION` / `SET CONSTRAINTS`.

- A slow or hung query no longer starves `ping`, `test_connection`,
  `initialize`, and the `cancel` notification for every connection (#167,
  #176). The plugin had 4 workers fed by a single shared queue; every RPC
  type shared them, so 4 stuck calls (any slow query up to the host's 120s
  timeout) blocked health checks and new connects to *other* databases —
  and the `cancel` notification that's meant to relieve the stuck workers
  queued behind them (a deadlock: only quitting Tabularis freed the
  workers). RPCs are now routed by method into three lanes before a worker
  pulls the line: a dedicated cancel lane (the relief valve, never queued
  behind the workers it relieves), a 2-worker lifecycle lane
  (`ping`/`test_connection`/`initialize`/`shutdown`), and the existing
  4-worker general lane. The `SHUTDOWN_GRACE` drain (#155) covers all
  lanes' worker handles.

### Known limitations

- A bare `[SET, SELECT]` or `[CREATE TEMP TABLE, SELECT]` batch on a
  **real** PgBouncer (`pool_mode = transaction`, not the direct +
  `pooler_mode = transaction` flag config) can still lose session state
  between autocommit statements (pre-existing on `main` — the backend is
  reassigned per transaction) and, in rare cases with non-builtin result
  column types under backend reassignment *during* the pre-resolve
  `prepare`, can hang (#175). Wrap such batches in explicit
  `BEGIN`/`COMMIT` to pin the backend. Tracked as a follow-up; the
  direct + `pooler_mode = transaction` config (the common use) is
  unaffected.

- A `SELECT` against a table with a composite type that is `ALTER TYPE`-ed
  (same OID, new field layout) on a recycled pooled connection can read a
  stale cached `Type` and produce wrong JSON for the composite's fields
  (#171). Pre-existing (a tokio-postgres/deadpool cache-invalidation
  limitation the plugin inherited); the rc.11/#168 pre-resolve widened the
  cache-warming window but did not introduce the bug. Tracked as a
  follow-up pending investigation of tokio-postgres's `clear_type_cache`
  API.

## [1.0.0-rc.11] - 2026-10-22

### Fixed

- Querying a table with a non-builtin PostgreSQL type (custom `enum`,
  composite, domain, range, or extension type like `citext` / Supabase
  `auth.uid`) no longer hangs or errors behind a transaction-mode pooler
  (PgBouncer `pool_mode = transaction`, Supavisor) — the rc.9/rc.10
  regression (#160, #163). Result-set queries on connections marked
  Pooler Mode = "Transaction" now take a pinned one-shot transaction path:
  `BEGIN` pins the backend, `DEALLOCATE ALL` clears stale server-side
  prepared statements, `prepare` resolves every result column's type
  while pinned (before the data fetch drains anything), and the #149
  unnamed-protocol `query_typed` runs the data fetch — so tokio-postgres's
  internal cached `typeinfo_statement` no longer collides across pooler
  backend reassignment. The one-shot client is discarded after each call,
  so its private type-cache handle (which `DEALLOCATE ALL` can't clear)
  dies with the connection. The whole sequence is bounded by a 90s
  plugin-side timeout (below the host's 120s call timeout) so a Supavisor
  stall surfaces as an actionable error instead of the opaque host timeout.

- The one-shot pool path skips the redundant startup-script preflight —
  the `post_create` hook already validates the script on the real
  connection with the same error shape, so the preflight would just open a
  second throwaway connection per query for nothing. Only affects the
  `pooler_mode = transaction` + `startup_script` intersection.

- `pooler_mode` is now trimmed before the transaction comparison (matching
  how `startup_script` is normalized in `build_pool`), so a hand-edited
  `connections.json` with surrounding whitespace can't silently bypass the
  pinned path.

### Changed

- `format_query_error` now emits the `max_prepared_statements > 0` +
  `pooler_mode` guidance on **all** 26000/42P05 errors, not only when
  `pooler_mode = transaction` is set. `is_transaction_pooler` now selects
  the phrasing (definitive when the host declared a transaction pooler,
  conditional when it didn't), so users on older hosts that can't send the
  field still see the hint.

### Security

- Bumped `rustls-webpki` (0.103.15 → 0.103.16) and `webpki-root-certs`
  (1.0.9 → 1.0.10) to clear the emSign root-cert advisories. `cargo audit`
  now reports zero vulnerabilities (only the pre-existing `chacha20`
  yanked-crate warning remains). The existing `rustls` 0.23.46 and
  `rustls-platform-verifier` 0.7.1 floors already accept the patched
  versions, so no `Cargo.toml` change was needed.

### Dependencies

- `tokio` 1.53.2 → 1.53.3 (#161).
- `sha2` 0.11.0 → 0.11.1 (#162).

## [1.0.0-rc.10] - 2026-10-16

### Fixed

- The plugin process no longer hangs forever when Tabularis quits while an
  RPC is in flight. After stdin EOF (the orphan path — the host is gone),
  in-flight RPCs now drain for at most 5s (`SHUTDOWN_GRACE`), then any
  worker still blocked on an unbounded await is force-cancelled via
  `handle.abort()`, which drops the held `deadpool::Object` and closes the
  DB socket server-side. `tokio-postgres`'s `connect_timeout` bounds only
  the socket-level connect, not the startup handshake, so it could not
  bound the pooler-accepts-but-never-responds case — only this shutdown
  deadline covers every hang mode (connect, handshake, query, pool wait)
  uniformly (#156, #155).

### Added

- `pooler_mode` field in `ConnectionParams` for PgBouncer transaction-mode
  pooler awareness. When the host declares `"transaction"` (companion:
  tabularis#1126), the plugin skips its own `prepare()` call for
  column discovery on empty result sets (eliminating the plugin's
  named-statement trigger) and rewrites SQLSTATE 26000/42P05 errors
  (from tokio-postgres's internal `typeinfo_statement` fired by non-builtin
  column types — custom enums, composites, extension types) with actionable
  guidance pointing to `max_prepared_statements > 0` in pgbouncer.ini.
  Backward-compatible: `pooler_mode` defaults to `None` — no behavior
  change until the host sends the field (#158, #157, tabularis#1097).

## [1.0.0-rc.9] - 2026-10-15

### Fixed

- Every query now uses the unnamed-statement protocol (`Parse("")` + `Bind` +
  `Describe` + `Execute` in a single flushed message) instead of minting a
  named prepared statement (`s{N}`) split across two `Sync`s. A
  transaction-mode connection pooler such as PgBouncer (`pool_mode =
  transaction`) can reassign the server connection between those two
  `Sync`s, so the `Bind` could land on a backend where the statement was
  never created — `ERROR: prepared statement "s42" does not exist`
  (SQLSTATE 26000). The unnamed statement has no name to go missing and
  lives only for the single extended-query sequence, so no reassignment can
  orphan it. This matches `psql`, which uses the unnamed statement by
  default for the same reason (#149, tabularis#1097).

### Changed

- Bumped `rustls` from 0.23.45 to 0.23.46 (#150).
- Bumped `serde_json` from 1.0.151 to 1.0.154 (#152).
- Bumped `uuid` from 1.27.0 to 1.28.0 (#153).

## [1.0.0-rc.8] - 2026-10-13

### Fixed

- `real`/`float4` values no longer show float noise in the grid (`89.9`
  instead of `89.9000015258789`). The shortest f32 decimal is used for
  display, with the two f32 midpoint values (`±7.038531e-26`) using the
  exact widened value so the JSON number round-trips to the stored f32.
  CRUD, BLOB, and foreign-key row matching cast the WHERE value to `real`
  when the column type is REAL/FLOAT4, so the shorter value still finds the
  row after the grid sends it back (#144).

- Selecting a schema prepends it onto the connection's base `search_path`
  instead of overwriting it, so queries against objects in a different
  schema still resolve. The base is captured once per connection at pool
  creation (after any startup script) into a custom GUC, making the
  prepend idempotent across recycled pooled connections — without it, a
  recycled connection would stack prior selections and leak a previous
  schema into object resolution (#51, #145).

- `numeric`/`decimal` columns generate with their precision and scale
  (`numeric(10,2)` instead of bare `numeric`) in CREATE TABLE DDL, and
  composite foreign keys preserve their column order via `WITH ORDINALITY`
  (#143).

### Added

- `get_materialized_view_definition` RPC method — the last
  `not_implemented` stub — now returns a materialized view's DDL. Filters
  by `relkind = 'm'` so a regular view or table name returns a clean
  "not found" error instead of wrong DDL (#32, #146).

## [1.0.0-rc.7] - 2026-10-08

### Fixed

- A failed connection attempt (wrong password) no longer poisons the pool
  cache: deadpool-postgres connects lazily, so a pool built with a wrong
  password was cached before the handshake was tested, and the cache key
  excluded the password, so the next request with the correct password
  reused the poisoned pool and failed until the 600s idle sweep dropped
  it. Three layered fixes: (1) `get_pool_client` evicts the cache entry
  on `pool.get()` failure, (2) `connection_key` folds a SHA-256 hash of
  the password so different passwords get distinct pools, (3)
  `format_pg_error` returns just the `severity: message` line instead of
  the full `tokio_postgres::Error` Debug dump that Tabularis showed
  verbatim in its password prompt (#132).

- When `connection_string` is set, the pool cache key now folds in a
  SHA-256 hash of the connection string (which carries the password) so
  two strings differing only by password get distinct pools, and TLS
  params + startup_script remain in the key since `build_pool` reads
  them unconditionally even when `connection_string` is set (#134).

- Non-object JSON-RPC input (arrays, numbers, strings, bools, null) now
  returns a `-32600 Invalid Request` response instead of being silently
  swallowed as a notification, which caused the client to hang (#135).

- Invalid JSON-RPC objects missing a string `method` field (e.g. `{}` or
  `{"id":1}`) now return `-32600 Invalid Request` instead of being
  silently swallowed as a notification (no `id`) or returning a
  misleading `-32601 Method Not Found` (has `id`) (#137).

### Changed

- Bumped `rustls-platform-verifier` from 0.7.0 to 0.7.1 (Windows/Android
  bugfixes) (#128).
- Bumped `uuid` from 1.26.1 to 1.27.0 (case-insensitive URN parsing, V7
  ordering fix) (#130).
- Bumped `tokio` from 1.53.1 to 1.53.2 (bugfixes in fs/sync/rt/time) (#131).

## [1.0.0-rc.6] - 2026-09-30

### Added

- `cancel` JSON-RPC notification handler: on a host plugin-call timeout,
  the host can send a fire-and-forget `cancel` notification (`{"method":
  "cancel","params":{"id":<request-id>}}`) and the plugin cancels the
  server-side statement via `pg_cancel_backend` instead of leaving the
  orphaned backend running. Each in-flight `execute_query`/
  `execute_query_batch`/`explain_query` registers a `CancelToken` keyed by
  request id and deregisters it when the query resolves, so a late or
  duplicate cancel for a reused id is a no-op. A true notification (no
  top-level `id`) gets no response, per JSON-RPC convention. Dormant until
  the host ships the notification (`tabularis#832`); until then this is a
  safe no-op addition — no existing RPC's behavior or response shape
  changes (#126).

### Fixed

- `get_columns`/`get_all_columns_batch`/`get_schema_snapshot` filtered out
  `NULL` column defaults case-sensitively (`d == "NULL"`), so a column
  declared `DEFAULT null` (lowercase — legal PostgreSQL) surfaced a
  spurious `default_value: "null"` where the built-in driver omits it.
  Now filters case-insensitively (`eq_ignore_ascii_case("null")`),
  matching the builtin exactly; the `NULL::<type>` cast prefix check
  stays case-sensitive as in the builtin (#122).

## [1.0.0-rc.5] - 2026-09-29

### Added

- `get_table_ddl` RPC method, returning a reconstructed `CREATE TABLE`
  statement for an existing table so the host's `dump_database` can write
  a schema-preserving dump for plugin-registered PostgreSQL connections
  instead of failing with "method not implemented" (#118).
- `get_schema_snapshot` RPC method — now implemented instead of returning
  "method not found", so the ER diagram window renders a schema's
  tables/columns/foreign keys in one round trip rather than falling back
  to the host's O(number of tables) composition of `get_tables` +
  `get_columns` + `get_foreign_keys` (tabularis#822). Mirrors the built-in
  driver's `get_schema_snapshot` byte-for-byte (#121).
- `get_all_columns_batch` and `get_all_foreign_keys_batch` RPC methods —
  the batch building blocks `get_schema_snapshot` composes, implemented
  alongside it for consistency. Each returns all columns/FKs for every
  base table in a schema in one query, grouped by table name (#121).
- `execute_query` and `execute_query_batch` accept a `session_id` and keep
  that session's connection when the statement or batch leaves an explicit
  transaction open, so `BEGIN`, the changes, a verifying `SELECT` and
  `COMMIT` can each be their own run instead of having to be one script.
  With a `session_id` the replies carry `{ result, in_transaction }` and
  `{ results, in_transaction }`; without one they stay the bare
  `QueryResult` and array, so older hosts are unaffected.
- `release_session` RPC method, called when the owning editor tab closes.

### Fixed

- A batch that left a transaction open returned its connection to the pool
  as-is. The pool recycles with `RecyclingMethod::Fast`, which resets
  nothing, so the next borrower inherited the open transaction and its
  locks. A connection is now always rolled back before it goes back.
- `.tabularium` split its single `name` field into a stable `id` slug
  (`postgresql`) and a human-readable `name` (`"PostgreSQL"`), per the
  Tabularium 0.14.0+ identity/display-name split. Existing registry slug
  and driver id are unchanged (#117).

## [1.0.0-rc.4] - 2026-09-17

### Added

- `routine_management` RPC methods — `build_routine_call_sql`,
  `routine_create_template`, `drop_routine` — are now implemented instead
  of falling back to generic, non-PostgreSQL SQL: invalid call syntax for
  functions with `OUT` params, a template missing `LANGUAGE plpgsql`/`$$`
  dollar-quoting, and a bare `DROP FUNCTION name` that can't disambiguate
  overloads (#84).

### Fixed

- PK-binding cluster in `bind_pk_value`/`build_pk_map_predicate`: NULL
  (#78), boolean (#79), and integer-shaped-string (#80) primary-key values
  either hit an `"Unsupported PK type"` catch-all or, for the string case,
  over-coerced a text column's value to `bigint` and tripped a type-mismatch
  error server-side. An empty `pk_map` also built a malformed trailing
  `WHERE` clause with no client-side error (#81). All four now match the
  builtin driver's binding behavior exactly.
- `extract.rs`'s type dispatch was restructured to the builtin's
  `Kind`-first shape (`Simple`/`Enum`/`Array`/`Range`/`Multirange`/`Domain`/
  `Composite`), closing out ~43 PostgreSQL types that previously decoded to
  `null` (or, for `JSONPATH`, a corrupted string): `BIT`/`VARBIT`,
  `MACADDR8`, `XID`/`CID`/`TID`/`XID8`, the eleven `REG*` object-reference
  types, all seven geometric types (`POINT`/`LSEG`/`BOX`/`POLYGON`/`PATH`/
  `LINE`/`CIRCLE`), full-text-search and introspection types
  (`TSVECTOR`/`TSQUERY`/`JSONPATH`/`XML`/`PG_LSN`/etc.), pgvector
  (`vector`/`halfvec`/`sparsevec`), `Kind::Multirange`, `Kind::Domain`,
  `Kind::Composite`, and arrays of all of the above — including arrays of
  the six built-in range types, found during the same pass (#82).
- Write-side binding (`bind_pg_string`) had no branches for pgvector
  literals, raw-SQL-function calls (e.g. `ST_GeomFromText(...)`), or bare
  WKT geometry literals — the first bound incorrectly as an `ARRAY[...]`
  constructor, the other two stored as literal text instead of executing
  server-side (#83).
- Startup scripts had no execution timeout — a script that blocks (an
  advisory lock, `pg_sleep`, a stalled host) wedged pool creation, and
  therefore every RPC needing that pool, indefinitely. Bounded to the
  same 30-second timeout the builtin driver uses (#85).
- Binding an empty JSON array (`[]`) produced the invalid SQL fragment
  `ARRAY[]`, which PostgreSQL rejects for lack of an inferred element
  type. Now binds as the canonical `'{}'` empty-array literal (#86).
- The BYTEA read path base64-encoded the entire value with no size cap and
  a hardcoded `application/octet-stream` MIME type — including a second,
  independently-drifted untruncated copy reachable only via array elements
  and composite fields. Both now cap at the same 10 KB preview size the
  builtin driver uses and sniff MIME from magic bytes (#87), and the same
  cap now applies to the internal planner-statistics blob types
  (`pg_mcv_list`, `pg_dependencies`, etc.), whose macro had drifted onto
  its own untruncated encoder discovered while fixing #87 (#106).
- `get_routines` unconditionally queried `pg_proc.prokind` (PostgreSQL 11+
  only), failing with `column "prokind" does not exist` against
  PostgreSQL 9.x/10. Falls back to the legacy `proisagg`/`proiswindow`
  columns below server version 110000, matching the builtin driver (#88).
- `explain_query` returned the raw EXPLAIN JSON directly instead of the
  `{ engine, format, payload, original_query }` shape the host's plugin
  adapter requires to classify a response as `Raw` — so every EXPLAIN
  result fell through to the generic parsed-plan renderer instead of the
  builtin driver's raw-payload path (#89).
- The rustls `CryptoProvider` was only installed as a side effect inside
  the `require`/`verify-ca` TLS branches; `verify-full` and the
  `prefer`/`allow`/`disable` fallthrough built their TLS config directly,
  with no such side effect, making provider installation depend on branch
  execution order rather than always running first (#90).

## [1.0.0-rc.3] - 2026-09-15

### Added

- Table and column comments (`COMMENT ON TABLE`/`COMMENT ON COLUMN`,
  stored in `pg_description`) are now exposed as an optional `comment`
  field in `get_tables`/`get_columns`, joined via `pg_class`/
  `pg_attribute`. Matches the builtin driver's parity fix. Additive —
  no manifest/capability change, no minimum runtime bump (#74, #76).

### Fixed

- `hstore` columns decoded to `null` instead of a JSON object, since
  `extract.rs`'s type dispatch had no arm for hstore's extension OID
  (which varies per installation and isn't a well-known Postgres
  type). Also fixes the write side: editing an hstore cell previously
  failed with "Cannot bind a JSON object to a non-JSON column" (#68,
  #69, #71).
- Arrays of any custom-OID element type — `enum[]`, `hstore[]`, and in
  practice every array type without a hardcoded fast-path (`numeric[]`,
  `date[]`, `json[]`, `money[]`, `inet[]`, etc.) — also decoded to
  `null`. Adds a generic array decoder that recurses per-element,
  matching the builtin driver's dispatch. Existing hardcoded array
  fast-paths (int2/int4/int8/float4/float8/bool/text/varchar) are
  unaffected (#71, #72).
- The hardcoded array fast-paths decoded the *entire* array to `null`
  if any single element was `NULL` (e.g. `ARRAY[1, NULL]` returned
  `null`, not `[1, null]`), since they decoded via `Vec<T>: FromSql`
  rather than `Vec<Option<T>>: FromSql`. Now preserves the `null` slot
  in its correct position (#73, #75).
- Running `SHOW search_path` (or any `CALL`) from the Console produced
  `syntax error at or near "LIMIT"`, because the Console always sends
  a `limit`/`page` param and pagination was unconditionally appending
  a SQL `LIMIT`/`OFFSET` clause — but PostgreSQL's `SHOW`/`CALL` syntax
  doesn't accept one. `SELECT`/`WITH`/`VALUES`/`TABLE`/`EXPLAIN` do
  accept it and keep real SQL-level pagination (multi-page browsing
  still works for CTEs); only `SHOW`/`CALL` fall back to capping rows
  client-side instead (#70, #77).
- A comment-headed query (e.g. `-- note\nSELECT 1`) was misclassified
  as not returning a result set, silently losing its row data through
  the execute()-and-discard path instead of the fetch-rows path (#77).

### Changed

- Bumped `rustls` from 0.23.43 to 0.23.45, fixing RUSTSEC-2026-0285
  (CVE-2025-61730) — a TLS 1.3 handshake message boundary check.
  Lockfile-only change, no `Cargo.toml` range change.

## [1.0.0-rc.2] - 2026-09-10

### Fixed

- Query, startup-script, and connection-establishment failures now surface
  PostgreSQL's real error message instead of the generic literal string
  `"db error"`. `tokio_postgres::Error`'s own `Display` impl prints that
  fallback for any server-side error (its `Kind::Db` arm); the actual
  message lives in the wrapped `DbError`, reachable via `.as_db_error()`.
  Every call site that stringified the error directly — query execution,
  startup scripts, blob fetch, `SET search_path`, and the connection
  handshake itself (bad database name, bad password) — lost that detail.
  Ports the built-in driver's `format_pg_error` helper and applies it at
  every affected site (#66, #67).

### Changed

- Bumped `rust_decimal` from 1.42.1 to 1.43.0 (backported fixes, perf
  improvements, clippy cleanup; lockfile-only change, no `Cargo.toml`
  range change). Verified `NUMERIC`/`Decimal` round-tripping — including
  negative values, the one behavioral fix in this release — against a
  live PostgreSQL container with no regressions (#65).

## [1.0.0-rc.1] - 2026-09-04

No code changes since `1.0.0-beta.10` — this tag promotes that build to the
`rc` channel. This repo tracks a builtin driver that keeps shipping its own
changes independently (TLS/pool-config semantics, wire-format coverage —
see `CLAUDE.md`'s "Parity gaps beyond SQL text"), so waiting for full
parity before calling a release candidate meant an indefinite `beta` line
chasing a moving target. `1.0.0-rc.1` is the checkpoint instead: parity
gaps discovered from here on are tracked as issues/patches on the `rc`
line (`rc.2`, `rc.3`, ...), not reasons to hold the channel back. New PRs
should use the `prerelease:rc` label going forward (see README's
"Contributing: PR Titles & Versioning").

## [1.0.0-beta.10] - 2026-09-04

### Added

- Configurable connection-pool size via a `poolMaxSize` driver setting
  (default 10, clamped to 1–64, parsed from u64/i64/string with zero/invalid
  → default). Ports the built-in `postgres` driver's configurable pool size
  (TabularisDB/tabularis#681) to the plugin. Motivated by pgBouncer
  (TabularisDB/tabularis#71): against a pooler you want a *small* client
  pool so you don't burn server slots, and previously the only option was a
  recompile. The setting is declared in `.tabularium` and received via the
  `initialize` RPC; an absent/invalid value degrades gracefully to the
  default (10), the built-in driver's pin.

### Fixed

- Restored pool-size parity with the built-in driver. `build_pool` never
  set `max_size`, so deadpool fell back to `PoolConfig::default()` =
  `logical_cores × 2` (up to ~32 on a 16-thread machine) — up to ~3× more
  backend connections per target than the built-in's pinned 10. Pools now
  default to 10 regardless of CPU count, the same gap the new setting
  addresses. The 82-test parity suite compares query results, not
  connection counts, so this latent gap went undetected.

### Changed

- Bumped `deadpool-postgres` from 0.14.1 to 0.14.2 (pulls `deadpool`
  0.12 → 0.13.1, `deadpool-runtime` 0.1 → 0.3.1, `getrandom` 0.2 → 0.4
  transitively). No public-API changes to the pool surface this plugin
  uses; 8/8 live-db tests confirm identical pool/connect/TLS/startup-script
  behavior.
- Bumped `uuid` from 1.24.1 to 1.26.0. Used only for `Uuid` parse/to-string
  in `binding.rs`/`extract.rs` (no generation, no new APIs); 8/8 live-db
  tests pass.
- Bumped `log` from 0.4.33 to 0.4.34.

### Documentation

- Added GitHub Issue Form templates (`.github/ISSUE_TEMPLATE/`) for the
  Tabularis builtin-to-plugin migration flow: `migration-failure.yml`,
  `capability-gap.yml`, and `bug_report.yml`, plus the `migration` and
  `capability-gap` labels. Pre-filled by the app via query params keyed on
  each form element's id; no separate release impact.

## [1.0.0-beta.9] - 2026-08-25

### Fixed

- Driver icon rendered invisible — `.tabularium`'s badge `color` and the
  icon SVG's `fill`/`stroke` were both `#336791`, so the mark had zero
  contrast against its own badge on every connection using this driver.
  Switched the icon to white so it reads against the fixed badge
  background regardless of app theme.

### Changed

- Bumped `uuid` from 1.24.0 to 1.24.1 (patch release fixing non-ASCII
  character handling in parse diagnostics; no behavioral overlap with
  this plugin's `v4` generation/serde usage in `binding.rs`/`extract.rs`).

## [1.0.0-beta.8] - 2026-08-19

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
- `ssl_mode=require`/`verify-ca`/`verify-full` silently allowed plaintext
  connections — `build_pool` never called `cfg.ssl_mode(...)` on the
  `deadpool_postgres::Config`, so the underlying `tokio_postgres::Config`
  kept its own default (`SslMode::Prefer`: negotiate TLS if offered, but
  accept plaintext otherwise) regardless of what this plugin's `ssl_mode`
  was actually set to. A user opting into "TLS or nothing" got an
  unencrypted connection with no error if the server couldn't/wouldn't
  negotiate TLS — a security-relevant gap, not just a correctness one.
  Added `resolve_ssl_mode`, mapping this plugin's `ssl_mode` strings to
  `deadpool_postgres::SslMode` to match the builtin driver's
  `build_postgres_configurations` mapping exactly (`disable`→`Disable`,
  `allow`/`prefer`→`Prefer`, `require`/`verify-ca`/`verify-full`→`Require`),
  and wired it into `build_pool`. Proved the bug and the fix with a live
  test against a real non-SSL PostgreSQL instance: `ssl_mode=require`
  connected successfully before this fix, and now correctly fails.
- `ssl_mode=require` validated the server certificate against the platform
  trust store instead of skipping validation entirely. `build_tls_connector`'s
  own doc comment already said `require` should force TLS "without
  certificate validation," but `needs_cert_validation` only matched
  `verify-ca`/`verify-full` — `require` fell through to the final
  `with_platform_verifier()` fallback, which does validate against the OS
  trust store, defeating the entire point of `require` vs. `verify-full`
  (the standard `require` use case is self-signed certs / private CAs the
  user hasn't configured `ssl_ca` for). Added `NoCertVerifier`, ported from
  the builtin driver's `src-tauri/src/pool_manager.rs::NoCertVerifier`
  (accepts any certificate unconditionally — no chain, hostname, or even
  TLS 1.2/1.3 signature verification), and routed `require` mode to it.
  Proved the bug and the fix live against a real self-signed-cert
  SSL-enabled PostgreSQL instance: `require` failed the TLS handshake
  before this fix, and now connects successfully; confirmed no regression
  in `verify-ca`/`verify-full` (still correctly validate) or `require`
  against a non-SSL server (still correctly fails, per the previous entry).
- `verify-ca` without an explicit `ssl_ca` file silently fell through to
  `with_platform_verifier()` — full OS-trust-store validation — instead of
  erroring, unlike the builtin driver's `build_postgres_tls_connector`,
  which requires an explicit CA file for this mode (platform roots aren't
  used, since macOS's strict EKU check rejects them) and errors clearly
  otherwise. `build_tls_connector` now returns the same class of error when
  `verify-ca` is set with no `ssl_ca`; `verify-full` without `ssl_ca`
  continues to use the platform verifier unchanged, since that's the
  documented distinction between the two modes.

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
