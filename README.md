<div align="center">
  <img src="https://raw.githubusercontent.com/TabularisDB/tabularis/main/public/logo-sm.png" width="120" height="120" alt="Tabularis logo" />
</div>

# tabularis-postgresql-plugin

<p align="center">

![Release](https://img.shields.io/github/release/TabularisDB/tabularis-postgresql-plugin.svg?style=flat)
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

> **Status: scaffolding only.** This repo currently holds only the repo-level
> basics (license, CI/release workflow shape, contributor docs). The actual
> plugin source (`Cargo.toml`, `src/`, `.tabularium` manifest) has not been
> migrated yet — it still lives at
> [`TabularisDB/tabularis` `plugins/postgres-plugin/`](https://github.com/TabularisDB/tabularis/tree/main/plugins/postgres-plugin)
> pending the CP-4 extraction (see
> [the migration plan](https://github.com/TabularisDB/tabularis/blob/main/.github/planning/postgres-plugin/02-phase-1-plugin-build.md#repo-extraction--timing-and-open-question)).
> The CI workflow in this repo will not pass until that source lands.

## Table of Contents

- [Features](#features)
- [Connection Configuration](#connection-configuration)
- [Supported PostgreSQL Data Types](#supported-postgresql-data-types)
- [Installation](#installation)
  - [Automatic (via Tabularis)](#automatic-via-tabularis)
  - [Manual Installation](#manual-installation)
- [How It Works](#how-it-works)
- [Supported Operations](#supported-operations)
- [Building from Source](#building-from-source)
- [Development](#development)
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

## Connection Configuration

| Parameter | Description | Required |
| ----------- | ------------- | ---------- |
| `host` | PostgreSQL server hostname | Yes (unless using `connection_string`) |
| `port` | PostgreSQL server port (default `5432`) | No |
| `database` | Database name to connect to | Yes (unless using `connection_string`) |
| `username` | Database user | Yes (unless using `connection_string`) |
| `password` | Database password | If required by the server |
| `ssl_mode` | `disable`, `require`, `verify-ca`, or `verify-full` | No |
| `ssl_ca` / `ssl_cert` / `ssl_key` | Paths to SSL certificate material | If using `verify-ca`/`verify-full` |
| `connection_string` | Full `postgres://user:pass@host:port/db` URL, as an alternative to the discrete fields above | No |

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

### Automatic (via Tabularis)

If your version of Tabularis supports plugin management, the PostgreSQL plugin
can be installed directly from the application.

### Manual Installation

1. Download the latest release for your platform from the
   [Releases page](https://github.com/TabularisDB/tabularis-postgresql-plugin/releases).
2. Extract the archive.
3. Copy `postgresql-plugin` (or `postgresql-plugin.exe` on Windows) and
   `.tabularium` into the Tabularis plugins directory:

| OS | Plugins Directory |
| --- | --- |
| **Linux** | `~/.local/share/tabularis/plugins/postgresql/` |
| **macOS** | `~/Library/Application Support/com.debba.tabularis/plugins/postgresql/` |
| **Windows** | `%APPDATA%\debba\tabularis\data\plugins\postgresql\` |

1. Restart Tabularis.

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
| `get_routines` / `get_routine_parameters` / `get_routine_definition` / `drop_routine` | Function/procedure metadata |
| `get_triggers` / `get_trigger_definition` / `create_trigger` / `drop_trigger` | Trigger lifecycle |
| `execute_query` / `execute_query_batch` / `explain_query` | Query execution, multi-statement batches, and query plans |
| `insert_record` / `update_record` / `delete_record` | Row-level CRUD with type-aware value binding |
| `get_create_table_sql` / `get_add_column_sql` / `get_alter_column_sql` / `get_create_index_sql` / `get_create_foreign_key_sql` / `drop_index` / `drop_foreign_key` | DDL generation and execution |
| `save_blob_to_file` / `fetch_blob_as_data_url` | BLOB (`bytea`) export and preview |

## Building from Source

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (edition 2021)
- A running PostgreSQL instance (for integration tests)

### Build

```bash
cargo build --release
```

The binary will be located at `target/release/postgresql-plugin`.

### Install Locally

```bash
just dev-install
```

## Development

### Running Tests

```bash
# Unit tests
cargo test

# Lint
cargo clippy --all-targets -- -D warnings

# Format
cargo fmt --all
```

### Manual JSON-RPC test via shell

```bash
echo '{"jsonrpc":"2.0","method":"test_connection","params":{"params":{"host":"127.0.0.1","port":5432,"username":"postgres","password":"password","database":"testdb"}},"id":1}' \
  | ./target/release/postgresql-plugin
```

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
