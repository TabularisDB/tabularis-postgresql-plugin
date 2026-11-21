# tokio-postgres 0.7.18: rows before type lookups

This is the `tokio-postgres` 0.7.18 crates.io source, retaining its
MIT/Apache-2.0 licenses. The normalized published `Cargo.toml` is used without
dependency changes; the crate's own tests and benches are not vendored. The
plugin's Cargo patch replaces only this crate: `deadpool-postgres`,
`tokio-postgres-rustls` and `postgres-types` stay on their published versions
and build against it.

Local changes relative to the published crate:

- `src/client.rs`: `Responses` can receive the rest of a request's answer ahead
  of its reading (`take_in_rest`) and hands it out afterwards, in order.
- `src/query.rs`: `query_typed` receives its rows before it looks up a column
  type the connection has not met.

## Why

`query_typed` sends Parse, Bind, Describe, Execute and Sync in one message, so
the server answers with the row description and then the rows. For a column of
a type that is neither built in nor cached on the connection (an enum, a domain,
a composite, an extension type such as `citext`), upstream looks the type up as
soon as it reads the row description: a further query, which the server answers
after the rows it is already sending.

The connection reads one request's answer at a time and hands it over through
a bounded channel. While `query_typed` waits for the lookup, nobody reads its
rows: once they fill the channel the connection stops reading the socket, the
lookup's answer stays behind them, and the query hangs. A small result fits in
the buffers and passes; a large one does not.

The plugin used to work around this by preparing every result-set statement
first, which resolves the types before any row is requested, at the cost of a
round trip per statement (#166, #168, #169). With the patch the rows are
received first and kept in memory, then the types are looked up, then the rows
are handed out. The prepare and the fetch can go out together and be waited on
at once (`exec_query_on_client` in `src/handlers/query.rs`).

## What it costs

Only a statement that returns a type the connection meets for the first time
takes the changed path. Its answer is held in memory until the lookups are
done. `Client::query_typed` collects every row anyway, so for the plugin the
peak is the same. A caller that streams through `query_typed_raw` loses the
streaming for that one statement.

## Checking it

`tests/live_db.rs` covers enum, composite, domain and extension-type results
through the plugin. To see the hang the patch removes, run a statement that
returns several megabytes of rows with a custom-type column on a fresh
connection, without preparing it first.

Remove this patch when the chosen upstream release looks types up without
holding up the rows of the query that needs them.
