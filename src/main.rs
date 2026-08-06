//! PostgreSQL plugin for Tabularis — JSON-RPC driver over stdin/stdout.
//!
//! # Protocol
//!
//! Reads newline-delimited JSON-RPC 2.0 requests from stdin and writes
//! responses (one JSON object per line) to stdout. All handler logic is
//! async (tokio) since the database pool requires an async runtime.
#![allow(dead_code)]

use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

mod binding;
#[cfg(test)]
mod binding_tests;
mod client;
mod error;
mod extract;
mod handlers;
mod models;
mod rpc;
mod utils;

#[tokio::main]
async fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut out = stdout;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = rpc::handle_line(trimmed).await;
        let mut body = match serde_json::to_string(&response) {
            Ok(s) => s,
            Err(err) => format!(
                "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":-32603,\"message\":\"serialization failed: {err}\"}},\"id\":null}}"
            ),
        };
        body.push('\n');
        if out.write_all(body.as_bytes()).await.is_err() {
            break;
        }
        let _ = out.flush().await;
    }
}
