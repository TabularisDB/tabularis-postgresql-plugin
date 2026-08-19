<div align="center">
  <img src="https://raw.githubusercontent.com/TabularisDB/tabularis/main/public/logo-sm.png" width="120" height="120" alt="Tabularis logo" />
  <img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/plus.svg" width="40" height="40" alt="plus" />
  <img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/postgresql-logo-3colors.png" width="120" height="120" alt="PostgreSQL logo" />
</div>

# tabularis-postgresql-plugin

<p align="center">

![Release](https://img.shields.io/github/v/release/TabularisDB/tabularis-postgresql-plugin?include_prereleases&style=flat)
![Downloads](https://img.shields.io/github/downloads/TabularisDB/tabularis-postgresql-plugin/total.svg?style=flat)
![CI](https://github.com/TabularisDB/tabularis-postgresql-plugin/workflows/CI/badge.svg)

</p>

A [PostgreSQL](https://www.postgresql.org/) plugin for [Tabularis](https://github.com/TabularisDB/tabularis),
the open-source database client.

This plugin connects Tabularis to PostgreSQL via a standalone Rust binary speaking
JSON-RPC 2.0 over stdio, replacing what was originally a built-in driver compiled
directly into the Tabularis application. It is byte-for-byte behaviorally
identical to that built-in driver, proven by an 82-test parity suite that runs
both drivers against the same live database and compares every response.

> **Requires Tabularis v0.20.0 or later.** This plugin relies on the plugin
> runtime introduced in that release; it will not load on earlier versions of
> [Tabularis](https://github.com/TabularisDB/tabularis).

## Table of Contents

- [Features](#features)
- [Screenshots](#screenshots)
- [Connection Configuration](#connection-configuration)
- [Supported PostgreSQL Data Types](#supported-postgresql-data-types)
- [Installation](#installation)
  - [From the Tabularium registry](#from-the-tabularium-registry)
  - [Manual Installation (alternative)](#manual-installation-alternative)
- [How It Works](#how-it-works)
- [Supported Operations](#supported-operations)
- [Known Limitations](#known-limitations)
- [Building from Source](#building-from-source)
- [Development](#development)
  - [Contributing: PR Titles & Versioning](#contributing-pr-titles--versioning)
- [Changelog](#changelog)
- [License](#license)

## Features

- **Connection** — Host/port or connection-string connections, with SSL (`disable`,
  `require`, `verify-ca`, `verify-full`) via `rustls`.
- **Schema Browsing** — Databases, schemas, tables, views, materialized views,
  routines (functions/procedures), and triggers.
- **Column & Key Metadata** — Column types (including enum labels and pgvector-style
  extension types via `udt_name` fallback), indexes (including composite/unique),
  foreign keys (including cross-schema).
- **Query Execution** — Arbitrary SQL with pagination, `EXPLAIN`/`EXPLAIN ANALYZE`,
  and multi-statement batches that share a single connection (so `BEGIN`/`COMMIT`,
  temp tables, and `SET` survive across statements).
- **Inline Editing** — Insert, update, and delete rows directly from the Tabularis
  data grid, with type-aware value binding (enum `CAST`, UUID, JSON/JSONB, arrays,
  temporal types, BLOB wire format).
- **DDL Generation** — `CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN` (including
  implicit-cast-compatible `TYPE` changes), `CREATE INDEX`, `ADD CONSTRAINT
  FOREIGN KEY`, plus the corresponding drops.
- **View & Trigger Lifecycle** — Create/alter/drop views, create/drop triggers,
  refresh materialized views.
- **BLOB Support** — Export a `bytea` column to a file or preview it as a
  MIME-sniffed data URL.
- **Cross-platform** — Pre-built binaries for Linux (x86_64/aarch64), macOS
  (x86_64/aarch64), and Windows (x86_64).

## Screenshots

<table>
<tr>
<td><img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/screenshots/02-database-picker.png" alt="PostgreSQL listed in the Choose a database picker" width="400" /><br />PostgreSQL in the database picker</td>
<td><img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/screenshots/03-connection-form.png" alt="PostgreSQL connection form" width="400" /><br />Connection configuration</td>
</tr>
<tr>
<td><img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/screenshots/06-schema-browser.png" alt="Schema browser showing tables, views, and materialized views" width="400" /><br />Multi-schema browsing</td>
<td><img src="https://raw.githubusercontent.com/TabularisDB/tabularis-postgresql-plugin/main/assets/screenshots/07-table-data.png" alt="Data grid with a PostgreSQL enum column value" width="400" /><br />Data grid with enum support</td>
</tr>
</table>

## Connection Configuration

| Parameter | Description | Required |
| ----------- | ------------- | ---------- |
| `host` | PostgreSQL server hostname | Yes (unless using `connection_string`) |
| `port` | PostgreSQL server port (default `5432`) | No |
| `database` | Database name to connect to | Yes (unless using `connection_string`) |
| `username` | Database user | Yes (unless using `connection_string`) |
| `password` | Database password | If required by the server |
| `ssl_mode` | `disable`, `require`, `verify-ca`, or `verify-full` | No |
| `ssl_ca` | Path to a custom CA bundle PEM file, used to validate the server's certificate under `verify-ca`/`verify-full` instead of the system trust store | No |
| `ssl_cert` | Path to a client certificate PEM file, for servers requiring mutual TLS (e.g. Google Cloud SQL). Must be set together with `ssl_key` | No |
| `ssl_key` | Path to the private key PEM file matching `ssl_cert`. Must be set together with `ssl_cert` | No |
| `connection_string` | Full `postgres://user:pass@host:port/db` URL, as an alternative to the discrete fields above | No |
| `startup_script` | SQL run on every new pooled connection (e.g. `SET search_path = ...`) before it's handed to a query | No |

## Supported PostgreSQL Data Types

| Category | Types |
| --- | --- |
| **Numeric** | SMALLINT, INTEGER, BIGINT, SERIAL, BIGSERIAL, REAL, DOUBLE PRECISION, NUMERIC, DECIMAL, MONEY |
| **String** | CHAR, VARCHAR, TEXT |
| **Date/Time** | DATE, TIME, TIMESTAMP, TIMESTAMPTZ, INTERVAL |
| **Other** | BOOLEAN, UUID, INET, CIDR, MACADDR |
| **JSON** | JSON, JSONB |
| **Binary** | BYTEA |

## Installation

### From the Tabularium registry

This plugin is published on the [Tabularium registry](https://registry.tabularis.dev) —
the same one the [DuckDB](https://github.com/TabularisDB/tabularis-duckdb-plugin)
and [Elasticsearch](https://github.com/TabularisDB/tabularis-elasticsearch-plugin)
plugins ship through. Install it from Tabularis's in-app plugin browser:
**Settings → Plugins**, search for **PostgreSQL**, and install.

If you point Tabularis at a different Tabularium instance via
`tabulariumRegistryUrl` in `config.json`, make sure that registry has
ingested this plugin's releases first.

### Manual Installation (alternative)

1. Download the latest release for your platform from the
   [Releases page](https://github.com/TabularisDB/tabularis-postgresql-plugin/releases) —
   look for the most recent `1.0.0-beta.N` tag; every release published so
   far, including the first public one, ships on the `beta` prerelease
   channel (see [Contributing: PR Titles & Versioning](#contributing-pr-titles--versioning)).
2. Extract the archive.
3. Copy `postgresql-plugin` (or `postgresql-plugin.exe` on Windows) and
   `.tabularium` into the Tabularis plugins directory:

   | OS | Plugins Directory |
   | --- | --- |
   | **Linux** | `~/.local/share/tabularis/plugins/postgresql/` |
   | **macOS** | `~/Library/Application Support/com.debba.tabularis/plugins/postgresql/` |
   | **Windows** | `%APPDATA%\debba\tabularis\data\plugins\postgresql\` |

4. Restart Tabularis.

## How It Works

The plugin is a standalone Rust binary that communicates with Tabularis through
**JSON-RPC 2.0 over stdio**:

1. Tabularis spawns the plugin as a child process.
2. Requests are sent as newline-delimited JSON-RPC messages to the plugin's `stdin`.
3. The plugin connects to PostgreSQL using [`tokio-postgres`](https://crates.io/crates/tokio-postgres)
   / [`deadpool-postgres`](https://crates.io/crates/deadpool-postgres) and writes
   responses to `stdout`.

Connection pools are cached in-process, keyed by `host:port:database:user`, so
repeated calls against the same target reuse an existing pool instead of
reconnecting.

## Supported Operations

| Method | Description |
| --- | --- |
| `test_connection` / `ping` | Verify connectivity with a lightweight `SELECT 1` |
| `get_databases` | List databases on the server |
| `get_schemas` | List schemas in the connected database |
| `get_tables` | List tables in a schema |
| `get_columns` / `get_view_columns` / `get_materialized_view_columns` | Column metadata for tables, views, and materialized views |
| `get_indexes` | Index metadata, including composite and unique indexes |
| `get_foreign_keys` | Foreign key metadata, including cross-schema references |
| `get_views` / `get_view_definition` / `create_view` / `alter_view` / `drop_view` | View lifecycle |
| `get_materialized_views` / `refresh_materialized_view` | Materialized view lifecycle |
| `get_routines` / `get_routine_parameters` / `get_routine_definition` | Function/procedure metadata |
| `get_triggers` / `get_trigger_definition` / `create_trigger` / `drop_trigger` | Trigger lifecycle |
| `execute_query` / `execute_query_batch` / `explain_query` | Query execution, multi-statement batches, and query plans |
| `insert_record` / `update_record` / `delete_record` | Row-level CRUD with type-aware value binding |
| `get_create_table_sql` / `get_add_column_sql` / `get_alter_column_sql` / `get_create_index_sql` / `get_create_foreign_key_sql` / `drop_index` / `drop_foreign_key` | DDL generation and execution |
| `save_blob_to_file` / `fetch_blob_as_data_url` | BLOB (`bytea`) export and preview |

## Known Limitations

A few RPC methods are registered but not yet implemented — they return a
"not implemented" error rather than real data: `get_schema_snapshot`,
`get_all_columns_batch`, `get_all_foreign_keys_batch`,
`get_materialized_view_definition`. Tracked in
[#32](https://github.com/TabularisDB/tabularis-postgresql-plugin/issues/32),
part of the [Phase 2](https://github.com/TabularisDB/tabularis-postgresql-plugin/issues/9)
set of planned PostgreSQL-specific features (sequences, JSONB inline
editing, extension-aware types, and more).

## Building from Source

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (edition 2021)
- [`just`](https://github.com/casey/just) (optional, wraps the common cargo invocations)
- A running PostgreSQL instance (for integration tests)

### Build

```bash
just build      # debug build
just release    # release build (what the GitHub Actions workflow ships)
```

Or directly with cargo:

```bash
cargo build --release
```

The binary will be located at `target/release/postgresql-plugin`.

### Install Locally

```bash
just dev-install   # build + copy binary and manifest into the Tabularis plugins dir
just uninstall     # remove the installed plugin
```

### Local Test Database

```bash
just demo-db        # postgres:16-alpine in Docker (postgres / password / testdb)
just demo-db-stop
```

## Development

### Running Tests

```bash
just test    # cargo test — unit tests for SQL builders, parsing, RPC
just lint    # clippy -D warnings
just fmt     # cargo fmt --all
```

### Manual JSON-RPC test via shell

```bash
echo '{"jsonrpc":"2.0","method":"test_connection","params":{"params":{"host":"127.0.0.1","port":5432,"username":"postgres","password":"password","database":"testdb"}},"id":1}' \
  | ./target/release/postgresql-plugin
```

### Contributing: PR Titles & Versioning

PR titles must follow [Conventional Commits](https://www.conventionalcommits.org/)
(`type: subject`, `type(scope): subject`, or `type!: subject` for a breaking
change) — enforced by CI on every PR. Add a `BREAKING CHANGE:` footer to the
PR description for breaking changes that don't fit cleanly into the title.

Every PR also needs exactly one `prerelease:alpha` / `prerelease:beta` /
`prerelease:rc` / `prerelease:stable` label, so CI knows which release
channel to target when suggesting the next version. There's no default —
CI fails with a clear error if the label is missing, rather than guessing.

| PR title type | Version impact |
| --- | --- |
| `feat` | minor |
| `fix`, `refactor`, `perf` | patch |
| `docs`, `style`, `chore`, `test`, `ci`, `build` | none — no release suggested |
| any type with `!` or a `BREAKING CHANGE:` footer | major |

CI posts a comment on the PR suggesting the next tag/version based on the
title's type and the `prerelease:*` label — informational only, nothing is
tagged or released automatically (yet). The suggestion updates (and marks
the previous suggestion as outdated) only when the underlying
classification actually changes, not on every edit to the title text.

### Tech Stack

- **Language:** Rust (edition 2021)
- **Database driver:** [tokio-postgres](https://crates.io/crates/tokio-postgres) + [deadpool-postgres](https://crates.io/crates/deadpool-postgres)
- **TLS:** [rustls](https://crates.io/crates/rustls) via [tokio-postgres-rustls](https://crates.io/crates/tokio-postgres-rustls)
- **Serialization:** serde + serde_json
- **Async runtime:** tokio
- **Protocol:** JSON-RPC 2.0 over stdio

## [Changelog](./CHANGELOG.md)

## Maintainers

- @aesslinger

## License

Apache-2.0.
