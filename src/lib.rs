//! PostgreSQL driver plugin for Tabularis.
//!
//! The crate is split into a thin library (this file plus the modules below)
//! and two binaries: `postgresql-plugin` (the real stdio JSON-RPC server) and
//! `test_plugin` (a local REPL that drives the same dispatch code). Keeping
//! the logic in a library is what lets the REPL exercise the exact same code
//! path.
#![allow(dead_code)]

pub mod binding;
#[cfg(test)]
mod binding_tests;
pub mod client;
pub mod error;
pub mod extract;
#[cfg(test)]
mod extract_tests;
pub mod handlers;
pub mod models;
pub mod rpc;
pub mod settings;
pub mod utils;
