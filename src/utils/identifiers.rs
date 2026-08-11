//! SQL identifier quoting utilities.

/// Quote a SQL identifier with double quotes, escaping any embedded quotes.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Produce a schema-qualified identifier: "schema"."name".
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}
