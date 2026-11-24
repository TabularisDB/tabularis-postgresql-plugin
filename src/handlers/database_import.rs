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
//! `ROLLBACK`, `COMMIT AND CHAIN`, `PREPARE TRANSACTION`, `COPY ... STDIN/STDOUT`
//! and psql meta-commands are rejected with an explanation.

use std::fs::File;
use std::io::BufReader;

use deadpool_postgres::Object as PgClient;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::cancel;
use crate::client;
use crate::handlers::query::{
    apply_search_path_local, leading_keywords, transaction_effect, TransactionEffect,
};
use crate::models::{inner_params, ConnectionParams};
use crate::rpc::{error_response, ok_response};
use crate::utils::sql_split::{SqlStatement, StatementSplitter};

/// Statements buffered between the file-reading thread and the executor.
const STATEMENT_QUEUE: usize = 64;
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
        params: ConnectionParams::from_value(raw),
        file_path,
        schema,
    })
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
            "standard_conforming_strings must stay on in an import: turning it off changes how string literals in the rest of the file are read",
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

/// `SET [SESSION|LOCAL] standard_conforming_strings = <anything but on>`.
fn disables_standard_conforming_strings(statement: &str) -> bool {
    const NAME: &str = "standard_conforming_strings";
    let words = leading_keywords(statement, 3);
    if words.first().map(String::as_str) != Some("SET") {
        return false;
    }
    let name_at = if matches!(words.get(1).map(String::as_str), Some("SESSION" | "LOCAL")) {
        2
    } else {
        1
    };
    if !words
        .get(name_at)
        .is_some_and(|w| w.eq_ignore_ascii_case(NAME))
    {
        return false;
    }
    let lower = statement.to_lowercase();
    let Some(at) = lower.find(NAME) else {
        return false;
    };
    let mut value = lower[at + NAME.len()..].trim_start();
    value = match value.strip_prefix('=') {
        Some(rest) => rest,
        None => value.strip_prefix("to").unwrap_or(value),
    };
    let value = value.trim().trim_matches(|c| c == '\'' || c == '"').trim();
    !matches!(
        value,
        "on" | "true" | "yes" | "1" | "t" | "y" | "tr" | "tru" | "ye" | "default"
    )
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

/// SQLSTATE 42601 "cannot insert multiple commands into a prepared statement"
/// can only mean the splitter handed over text holding more than one command;
/// say so instead of leaving the user with a bare syntax error.
pub(crate) fn statement_failure_reason(sqlstate: &str, formatted: &str) -> String {
    if sqlstate == "42601" && formatted.contains("multiple commands") {
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

/// Run just before `COMMIT`. On a connection behind a transaction pooler the
/// backend outlives this import, and a dump's own session-level settings
/// (`set_config('search_path', '', false)`, `SET ROLE`, ...) would otherwise
/// leak to the next client (`RESET ALL` skips `ROLE` and `SESSION
/// AUTHORIZATION`; resetting the latter clears both). `COMMIT` has to stay the last statement on the
/// pinned backend, so the durability setting is re-applied after the reset.
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

    let file = File::open(&req.file_path)
        .map_err(|e| format!("Failed to open SQL file '{}': {e}", req.file_path))?;
    let (sender, mut receiver) = mpsc::channel::<Item>(STATEMENT_QUEUE);
    // Statements stream through a bounded channel, so a multi-gigabyte dump
    // never sits in memory and a failed import stops the reader at once (the
    // dropped receiver fails its next send).
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
    let cancel_guard = cancel::CancelGuard::register(id.as_u64(), &pg_client, req.params.clone());

    pg_client
        .batch_execute("BEGIN")
        .await
        .map_err(|e| client::format_pg_error(&e))?;

    let outcome = match execute_all(&pg_client, &req.schema, &mut receiver, &cancel_guard).await {
        Ok(executed) => restore_session(&pg_client).await.map(|()| executed),
        Err(error) => Err(error),
    };
    match outcome {
        Ok(executed) => {
            pg_client.batch_execute("COMMIT").await.map_err(|e| {
                commit_error_message(
                    executed,
                    &client::format_pg_error(&e),
                    e.as_db_error().is_some(),
                )
            })?;
            Ok(executed)
        }
        Err(error) => {
            let _ = pg_client.batch_execute("ROLLBACK").await;
            Err(error)
        }
    }
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
    schema: &str,
    receiver: &mut mpsc::Receiver<Item>,
    cancel_guard: &cancel::CancelGuard,
) -> Result<usize, String> {
    apply_search_path_local(pg_client, schema).await?;
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
            return Err(format!(
                "Import cancelled before statement {} ({executed} statements ran); nothing was imported",
                ordinal + 1
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
                let sqlstate = e.code().map(|c| c.code()).unwrap_or("");
                statement_error(
                    ordinal,
                    statement.line,
                    &statement_failure_reason(sqlstate, &client::format_pg_error(&e)),
                    &statement.text,
                )
            })?;
        executed += 1;
    }
    Ok(executed)
}

#[cfg(test)]
#[path = "database_import_tests.rs"]
mod database_import_tests;
