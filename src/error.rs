//! Plugin error types.

use std::fmt;

#[derive(Debug)]
pub enum PluginError {
    Connection(String),
    Query(String),
    Internal(String),
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "connection error: {msg}"),
            Self::Query(msg) => write!(f, "query error: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for PluginError {}
