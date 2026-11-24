//! Transactional SQL file import (`import_database_sql`).
//!
//! Runs a whole `.sql` file in one transaction on a dedicated connection:
//! the first failing statement rolls everything back and the error names the
//! statement number, its line and its text.
//!
//! The connection is a one-shot (never returned to the shared pool) for two
//! reasons: a dump's own session-level `SET` / `set_config(..., false)` would
//! otherwise survive `COMMIT` and leak into later queries on a pooled
//! connection, and behind a transaction pooler the explicit `BEGIN` pins one
//! backend for the whole import.
//!
//! The import owns the transaction. `BEGIN`/`COMMIT` lines in the file are
//! skipped (honoring a mid-file `COMMIT` would silently end atomicity), while
//! `ROLLBACK`, `COMMIT AND CHAIN`, `PREPARE TRANSACTION`, `COPY ... STDIN/STDOUT`,
//! anything that could turn `standard_conforming_strings` off, and psql
//! meta-commands (`\restrict`/`\unrestrict` excepted, which are skipped) are
//! rejected with an explanation. [`import_on_client`] holds the transaction
//! body so it can be driven against any connection.

use std::fs::File;
use std::io::BufReader;

use deadpool_postgres::Object as PgClient;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::cancel;
use crate::client;
use crate::handlers::query::{
    leading_keywords, search_path_local_sql, transaction_effect, TransactionEffect,
};
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::utils::sql_split::{SqlStatement, StatementSplitter};

/// Statements buffered between the file-reading thread and the executor.
const STATEMENT_QUEUE: usize = 8;
/// Longest slice of a failing statement echoed back in an error message.
const MAX_ERROR_QUERY_CHARS: usize = 1000;

pub async fn import_database_sql(id: Value, request: &Value) -> Value {
    match run(&id, request).await {
        Ok(statements) => ok_response(id, json!({ "statements": statements })),
        Err(error) => error_response(id, -32603, &error),
    }
}

#[derive(Debug)]
pub(crate) struct ImportRequest {
    pub params: ConnectionParams,
    pub file_path: String,
    pub schema: String,
}

pub(crate) fn parse_request(request: &Value) -> Result<ImportRequest, String> {
    let raw = inner_params(request);
    if raw.get("read_only").and_then(Value::as_bool) == Some(true) {
        return Err("Connection is read-only".into());
    }
    if request.get("create_database").and_then(Value::as_bool) == Some(true) {
        return Err("This driver does not support creating a database during import".into());
    }
    let file_path = request
        .get("file_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or("file_path is required")?
        .to_string();
    let schema = request
        .get("schema")
        .and_then(Value::as_str)
        .filter(|schema| !schema.trim().is_empty())
        .unwrap_or("public")
        .to_string();
    Ok(ImportRequest {
        params: resolve_target(ConnectionParams::from_value(raw))?,
        file_path,
        schema,
    })
}

/// A legacy connection string names a database of its own, and the pool takes
/// its database from the URI whenever one is set. The host sends the database
/// the user picked in `database`, so that one has to win, or the dump lands in
/// the URI's database and still reports success. The URI keeps supplying
/// host, port and credentials, exactly as the pool reads them (the same rule
/// `database_operation` applies).
fn resolve_target(mut params: ConnectionParams) -> Result<ConnectionParams, String> {
    let Some(uri) = params
        .connection_string
        .clone()
        .filter(|uri| !uri.trim().is_empty())
    else {
        return Ok(params);
    };
    let parsed: tokio_postgres::Config = uri
        .parse()
        .map_err(|e| format!("Invalid connection string: {e}"))?;
    params.host = parsed.get_hosts().first().and_then(|host| match host {
        tokio_postgres::config::Host::Tcp(host) => Some(host.clone()),
        #[cfg(unix)]
        tokio_postgres::config::Host::Unix(_) => None,
    });
    params.port = parsed.get_ports().first().copied();
    params.username = parsed.get_user().map(str::to_string);
    params.password = parsed
        .get_password()
        .map(|password| String::from_utf8_lossy(password).into_owned());
    if params.database.as_deref().is_none_or(str::is_empty) {
        params.database = parsed.get_dbname().map(str::to_string);
    }
    params.connection_string = None;
    Ok(params)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Run,
    Skip,
    Reject(&'static str),
}

pub(crate) fn classify(statement: &str) -> Action {
    let trimmed = statement.trim_start();
    if trimmed.starts_with('\\') {
        let command = trimmed.split_whitespace().next().unwrap_or("");
        return match command {
            "\\restrict" | "\\unrestrict" => Action::Skip,
            _ => Action::Reject(
                "psql meta-commands (such as \\connect or \\i) are not supported in an import",
            ),
        };
    }

    if disables_standard_conforming_strings(statement) {
        return Action::Reject(
            "standard_conforming_strings must stay on in an import: setting it to anything but on, resetting it (RESET or RESET ALL) or set_config on it changes how string literals in the rest of the file are read",
        );
    }

    let first = leading_keywords(statement, 1)
        .into_iter()
        .next()
        .unwrap_or_default();
    match transaction_effect(statement) {
        TransactionEffect::Opens => return Action::Skip,
        TransactionEffect::Closes => {
            return match first.as_str() {
                "COMMIT" | "END" => Action::Skip,
                "ROLLBACK" | "ABORT" => Action::Reject(
                    "ROLLBACK is not allowed in an import: the import is one transaction that already rolls back on error",
                ),
                _ => Action::Reject(
                    "PREPARE TRANSACTION is not allowed in an import: it would detach the import's transaction",
                ),
            }
        }
        TransactionEffect::Chains => {
            return Action::Reject(
                "AND CHAIN is not allowed in an import: the import is one transaction",
            )
        }
        TransactionEffect::None => {}
    }

    if first == "COPY" {
        let words = keywords_outside_quotes(statement);
        let uses_client_stream = words
            .windows(2)
            .any(|w| (w[0] == "FROM" && w[1] == "STDIN") || (w[0] == "TO" && w[1] == "STDOUT"));
        if uses_client_stream {
            return Action::Reject(
                "COPY ... FROM STDIN / TO STDOUT is not supported in an import; re-export the data as INSERT statements (pg_dump --inserts)",
            );
        }
    }
    Action::Run
}

/// Whether `statement` could turn `standard_conforming_strings` off for the
/// rest of the import (the splitter reads plain strings with literal
/// backslashes, which is only right while it is on): `SET` to anything but
/// `on`/`true`/`yes`/`1` (so also `DEFAULT` and `FROM CURRENT`, which revert to
/// a database default that may be `off`), `RESET` of it, `RESET ALL`, or
/// `set_config` naming it. Comments are ignored; the name may be quoted.
fn disables_standard_conforming_strings(statement: &str) -> bool {
    const NAME: &str = "standard_conforming_strings";
    let text = without_comments(statement).to_lowercase();
    if !text.contains(NAME) && !text.trim_start().starts_with("reset") {
        return false;
    }
    let first = leading_keywords(statement, 1)
        .into_iter()
        .next()
        .unwrap_or_default();
    match first.as_str() {
        "SET" => match guc_name_tail(&text, "set", NAME) {
            Some(tail) => !is_on_value(tail),
            None => false,
        },
        "RESET" => {
            let rest = text.trim_start().trim_start_matches("reset").trim_start();
            rest.starts_with("all") && !rest[3..].starts_with(|c: char| c.is_alphanumeric())
                || guc_name_tail(&text, "reset", NAME).is_some()
        }
        _ => {
            text.contains(NAME)
                && keywords_outside_quotes(statement)
                    .iter()
                    .any(|word| word == "SET_CONFIG")
        }
    }
}

/// For `<verb> [session|local] [\"]name[\"] <tail>`, the tail; `None` when the
/// statement does not name `name` directly after the verb.
fn guc_name_tail<'a>(text: &'a str, verb: &str, name: &str) -> Option<&'a str> {
    let mut rest = text.trim_start().strip_prefix(verb)?.trim_start();
    for scope in ["session", "local"] {
        if let Some(after) = rest.strip_prefix(scope) {
            if after.starts_with(char::is_whitespace) {
                rest = after.trim_start();
                break;
            }
        }
    }
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let rest = rest.strip_prefix(name)?;
    Some(rest.strip_prefix('"').unwrap_or(rest))
}

/// Whether the text after the GUC name in a `SET` assigns a true value:
/// `= on`, `TO 'on'`, `= true` and the other spellings PostgreSQL accepts.
fn is_on_value(tail: &str) -> bool {
    let tail = tail.trim_start();
    let value = match tail.strip_prefix('=') {
        Some(rest) => rest,
        None => match tail.strip_prefix("to") {
            Some(rest)
                if rest.starts_with(|c: char| c.is_whitespace() || c == '\'' || c == '"') =>
            {
                rest
            }
            _ => return false,
        },
    };
    let value = value
        .trim()
        .trim_end_matches(';')
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .trim();
    matches!(
        value,
        "on" | "true" | "yes" | "1" | "t" | "tr" | "tru" | "y" | "ye"
    )
}

/// `sql` with `--` and (nested) `/* */` comments replaced by a space. Quoted
/// strings and identifiers are kept intact so a comment marker inside one is
/// not mistaken for a comment.
fn without_comments(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let mut depth = 1;
                i += 2;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.push(' ');
            }
            '\'' | '"' => {
                out.push(c);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            out.push(c);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Uppercased words of `sql`, skipping comments, string literals, quoted
/// identifiers and dollar-quoted bodies entirely.
fn keywords_outside_quotes(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() || c == '_' {
            word.push(c.to_ascii_uppercase());
            i += 1;
            continue;
        }
        if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let mut depth = 1;
                i += 2;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            '\'' | '"' => {
                let escapes = c == '\''
                    && i > 0
                    && matches!(chars[i - 1], 'e' | 'E')
                    && !(i > 1 && (chars[i - 2].is_alphanumeric() || chars[i - 2] == '_'));
                i += 1;
                while i < chars.len() {
                    if escapes && chars[i] == '\\' {
                        i += 2;
                    } else if chars[i] == c {
                        if chars.get(i + 1) == Some(&c) {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            '$' if i > 0
                && (chars[i - 1].is_alphanumeric() || matches!(chars[i - 1], '_' | '$')) =>
            {
                i += 1;
            }
            '$' => {
                let mut end = i + 1;
                while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let valid_tag = chars.get(end) == Some(&'$')
                    && !chars.get(i + 1).is_some_and(|c| c.is_ascii_digit());
                if valid_tag {
                    let tag = &chars[i..=end];
                    i = end + 1;
                    while i < chars.len() && !chars[i..].starts_with(tag) {
                        i += 1;
                    }
                    i = (i + tag.len()).min(chars.len());
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Message for a failed `COMMIT`. A server-side rejection (deferred constraint
/// violation) means PostgreSQL rolled the transaction back; a transport error
/// leaves the outcome unknown, so it must not claim either way.
pub(crate) fn commit_error_message(executed: usize, reason: &str, server_rejected: bool) -> String {
    if server_rejected {
        format!("COMMIT failed, nothing was imported ({executed} statements ran): {reason}")
    } else {
        format!(
            "COMMIT failed and the outcome is unknown ({executed} statements ran): {reason}. \
             Check the target database before retrying"
        )
    }
}

/// The full error text for a statement the server rejected.
pub(crate) fn describe_statement_failure(
    ordinal: usize,
    line: usize,
    error: &tokio_postgres::Error,
    text: &str,
) -> String {
    let sqlstate = error.code().map(|c| c.code()).unwrap_or("");
    statement_error(
        ordinal,
        line,
        &statement_failure_reason(sqlstate, &client::format_pg_error(error)),
        text,
    )
}

/// The full error text for a failed `COMMIT`: a server rejection means
/// PostgreSQL rolled the transaction back, anything else (a dropped
/// connection) leaves the outcome unknown.
pub(crate) fn describe_commit_failure(executed: usize, error: &tokio_postgres::Error) -> String {
    commit_error_message(
        executed,
        &client::format_pg_error(error),
        error.as_db_error().is_some(),
    )
}

/// Message for a deferred constraint that fails when the import forces the
/// pending checks to run just before `COMMIT`. The transaction is rolled back.
pub(crate) fn deferred_check_error_message(executed: usize, reason: &str) -> String {
    format!(
        "Deferred constraint check failed, nothing was imported ({executed} statements ran): {reason}"
    )
}

/// Message for a cancel noticed between statements or just before `COMMIT`.
pub(crate) fn cancelled_message(executed: usize, before: &str) -> String {
    format!("Import cancelled before {before} ({executed} statements ran); nothing was imported")
}

/// SQLSTATE 42601 "cannot insert multiple commands into a prepared statement"
/// can only mean the splitter handed over text holding more than one command;
/// say so instead of leaving the user with a bare syntax error.
pub(crate) fn statement_failure_reason(sqlstate: &str, formatted: &str) -> String {
    if sqlstate == "25001" && formatted.contains("must be called before any query") {
        format!(
            "{formatted} (the import has already started its transaction, so the file cannot \
             change transaction characteristics such as the isolation level)"
        )
    } else if sqlstate == "42601" && formatted.contains("multiple commands") {
        format!(
            "{formatted} (the statement could not be split into a single command; \
             please report this file's construct as a bug)"
        )
    } else {
        formatted.to_string()
    }
}

/// What the import reader sends: a statement, `Ok(None)` as the explicit end
/// of the file, or the reader's error. A channel that closes without the end
/// marker means the reader died, which must never look like a complete file.
pub(crate) type Item = Result<Option<SqlStatement>, String>;

pub(crate) async fn next_item(
    receiver: &mut mpsc::Receiver<Item>,
) -> Result<Option<SqlStatement>, String> {
    match receiver.recv().await {
        Some(item) => item,
        None => Err("The SQL file reader stopped unexpectedly; nothing was imported".into()),
    }
}

/// Run just before `COMMIT`, after the deferred constraint checks have fired
/// (they run under the import's own state). On a connection behind a
/// transaction pooler the backend outlives this import, and a dump's own
/// session-level settings (`set_config('search_path', '', false)`, `SET ROLE`,
/// ...) would otherwise leak to the next client (`RESET ALL` skips `ROLE` and
/// `SESSION AUTHORIZATION`; resetting the latter clears both). `COMMIT` has to
/// stay the last statement on the pinned backend, so the durability setting is
/// re-applied after the reset.
pub(crate) fn session_restore_statements() -> &'static [&'static str] {
    &[
        "RESET ALL",
        "RESET SESSION AUTHORIZATION",
        "SET LOCAL synchronous_commit = OFF",
    ]
}

pub(crate) fn statement_error(index: usize, line: usize, reason: &str, text: &str) -> String {
    let total = text.chars().count();
    let shown: String = text.chars().take(MAX_ERROR_QUERY_CHARS).collect();
    let query = if total > MAX_ERROR_QUERY_CHARS {
        format!(
            "{shown}… [truncated, {} more characters]",
            total - MAX_ERROR_QUERY_CHARS
        )
    } else {
        shown
    };
    format!("Error at statement {index} (line {line}): {reason}\nQuery: {query}")
}

async fn run(id: &Value, request: &Value) -> Result<usize, String> {
    let req = parse_request(request)?;
    // Claim the request id before connecting: a `cancel` that arrives while
    // the connection is still being set up is remembered, not dropped.
    let cancel_guard = cancel::CancelGuard::reserve(id.as_u64());

    let file = open_sql_file(&req.file_path).await?;
    let (sender, mut receiver) = mpsc::channel::<Item>(STATEMENT_QUEUE);
    // Statements stream through a small bounded channel, so a failed import
    // stops the reader at once (the dropped receiver fails its next send) and
    // only a few statements wait in memory ahead of the executor.
    tokio::task::spawn_blocking(move || {
        let mut splitter = StatementSplitter::new(BufReader::with_capacity(128 * 1024, file));
        loop {
            let item = splitter.next_statement();
            let last = !matches!(item, Ok(Some(_)));
            if sender.blocking_send(item).is_err() || last {
                return;
            }
        }
    });

    let pg_client = client::oneshot_pool_client(&req.params).await?;
    cancel_guard.attach(&pg_client, req.params.clone());

    import_on_client(&pg_client, &req.schema, &mut receiver, &cancel_guard).await
}

/// Open `path` for reading. Only a regular file is accepted: a FIFO would
/// block the open and a device such as `/dev/zero` would never end. The open
/// runs off the async threads for the same reason.
async fn open_sql_file(path: &str) -> Result<File, String> {
    let owned = path.to_string();
    tokio::task::spawn_blocking(move || {
        let open_error = |e: std::io::Error| format!("Failed to open SQL file '{owned}': {e}");
        let metadata = std::fs::metadata(&owned).map_err(open_error)?;
        if !metadata.is_file() {
            return Err(format!(
                "Failed to open SQL file '{owned}': not a regular file"
            ));
        }
        File::open(&owned).map_err(open_error)
    })
    .await
    .map_err(|e| format!("Failed to open SQL file '{path}': {e}"))?
}

/// The transaction body: `BEGIN`, session setup, every statement from
/// `receiver`, the deferred constraint checks, the session reset and `COMMIT`.
/// Any error rolls everything back. Takes the connection so it can be driven
/// against any client, not only the one-shot `run` opens.
pub(crate) async fn import_on_client(
    pg_client: &PgClient,
    schema: &str,
    receiver: &mut mpsc::Receiver<Item>,
    cancel_guard: &cancel::CancelGuard,
) -> Result<usize, String> {
    // Read the base search_path before `BEGIN`: the read is a query, and a query
    // takes the transaction's snapshot, after which a file's own
    // `SET TRANSACTION ISOLATION LEVEL` would be refused.
    let search_path_sql = search_path_local_sql(pg_client, schema).await?;
    pg_client
        .batch_execute("BEGIN")
        .await
        .map_err(|e| client::format_pg_error(&e))?;

    let outcome = match execute_all(pg_client, &search_path_sql, receiver, cancel_guard).await {
        Ok(executed) => finish(pg_client, executed, cancel_guard).await,
        Err(error) => Err(error),
    };
    match outcome {
        Ok(executed) => {
            pg_client
                .batch_execute("COMMIT")
                .await
                .map_err(|e| describe_commit_failure(executed, &e))?;
            Ok(executed)
        }
        Err(error) => {
            let _ = pg_client.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
}

/// Everything between the last statement and `COMMIT`. A cancel that landed
/// while the backend was idle is only seen here, and a deferred constraint is
/// checked while the import's own session state is still in place.
async fn finish(
    pg_client: &PgClient,
    executed: usize,
    cancel_guard: &cancel::CancelGuard,
) -> Result<usize, String> {
    if cancel_guard.is_cancelled() {
        return Err(cancelled_message(executed, "COMMIT"));
    }
    pg_client
        .batch_execute("SET CONSTRAINTS ALL IMMEDIATE")
        .await
        .map_err(|e| deferred_check_error_message(executed, &client::format_pg_error(&e)))?;
    restore_session(pg_client).await?;
    if cancel_guard.is_cancelled() {
        return Err(cancelled_message(executed, "COMMIT"));
    }
    Ok(executed)
}

/// The extended protocol takes exactly one command per statement, so a
/// splitter mistake becomes a server error instead of a second command (a
/// stray COMMIT among them) running unchecked.
pub(crate) async fn run_statement(
    pg_client: &PgClient,
    text: &str,
) -> Result<u64, tokio_postgres::Error> {
    pg_client.execute_typed(text, &[]).await
}

async fn restore_session(pg_client: &PgClient) -> Result<(), String> {
    for statement in session_restore_statements() {
        pg_client.batch_execute(statement).await.map_err(|e| {
            format!(
                "Import cleanup failed ({statement}): {}",
                client::format_pg_error(&e)
            )
        })?;
    }
    Ok(())
}

async fn execute_all(
    pg_client: &PgClient,
    search_path_sql: &str,
    receiver: &mut mpsc::Receiver<Item>,
    cancel_guard: &cancel::CancelGuard,
) -> Result<usize, String> {
    pg_client
        .batch_execute(search_path_sql)
        .await
        .map_err(|e| format!("Failed to set search_path: {}", client::format_pg_error(&e)))?;
    for setup in [
        // The splitter reads plain strings with literal backslashes; make the
        // server agree even when the database default is `off`.
        "SET LOCAL standard_conforming_strings = on",
        "SET CONSTRAINTS ALL DEFERRED",
        "SET LOCAL synchronous_commit = OFF",
    ] {
        pg_client.batch_execute(setup).await.map_err(|e| {
            format!(
                "Import setup failed ({setup}): {}",
                client::format_pg_error(&e)
            )
        })?;
    }

    let mut ordinal = 0;
    let mut executed = 0;
    while let Some(statement) = next_item(receiver).await? {
        // `pg_cancel_backend` is discarded while the backend is idle between
        // statements, so a cancel that lands there is only seen here.
        if cancel_guard.is_cancelled() {
            return Err(cancelled_message(
                executed,
                &format!("statement {}", ordinal + 1),
            ));
        }
        ordinal += 1;
        match classify(&statement.text) {
            Action::Skip => continue,
            Action::Reject(reason) => {
                return Err(statement_error(
                    ordinal,
                    statement.line,
                    reason,
                    &statement.text,
                ))
            }
            Action::Run => {}
        }
        run_statement(pg_client, &statement.text)
            .await
            .map_err(|e| {
                describe_statement_failure(ordinal, statement.line, &e, &statement.text)
            })?;
        executed += 1;
    }
    Ok(executed)
}

#[cfg(test)]
#[path = "database_import_tests.rs"]
mod database_import_tests;
