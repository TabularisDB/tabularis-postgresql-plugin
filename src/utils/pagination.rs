//! Pagination math for LIMIT/OFFSET queries.
//!
//! `build_paginated_query` mirrors the builtin driver's behavior in
//! `src-tauri/src/drivers/common/query.rs`: strip any trailing user-supplied
//! `LIMIT`/`OFFSET`, honor the user's LIMIT as a cap across pages, and append
//! the plugin's own pagination clause. ORDER BY is left in place (not wrapped
//! in a subquery) so table-qualified column references stay valid.
//!
//! This is a simpler whitespace/token scan than the builtin's full quote- and
//! comment-aware tokenizer — it correctly handles the common case (a plain
//! trailing `LIMIT n` / `LIMIT n OFFSET m`) but does not defend against SQL
//! comments after the clause or identifiers that literally are `LIMIT`/`OFFSET`
//! tokens inside quotes. Sufficient for the current parity test corpus;
//! revisit if a query pattern breaks this.

/// Compute the SQL LIMIT and OFFSET for a given page and page size.
/// Pages are 1-indexed.
pub fn limit_offset(page: u32, page_size: u32) -> (u32, u32) {
    let offset = (page.saturating_sub(1)) * page_size;
    (page_size, offset)
}

/// Split a query into whitespace-separated tokens, tracking each token's
/// starting byte offset in the original string.
fn tokenize_with_pos(sql: &str) -> Vec<(&str, usize)> {
    let mut tokens = Vec::new();
    let mut idx = 0;
    for part in sql.split_whitespace() {
        // Find this token's actual position (split_whitespace doesn't give us
        // offsets directly).
        let start = sql[idx..].find(part).map(|p| idx + p).unwrap_or(idx);
        idx = start + part.len();
        tokens.push((part, start));
    }
    tokens
}

/// Strip a trailing `LIMIT <n>` and/or `OFFSET <n>` clause from the query,
/// returning the query text with that clause removed.
fn strip_limit_offset(query: &str) -> String {
    let trimmed = query.trim_end().trim_end_matches(';').trim_end();
    let tokens = tokenize_with_pos(trimmed);
    let mut end = tokens.len();

    if end >= 2
        && tokens[end - 2].0.to_uppercase() == "OFFSET"
        && tokens[end - 1].0.parse::<u64>().is_ok()
    {
        end -= 2;
    }

    if end >= 2
        && tokens[end - 2].0.to_uppercase() == "LIMIT"
        && tokens[end - 1].0.parse::<u64>().is_ok()
    {
        end -= 2;
    }

    if end == tokens.len() {
        return trimmed.to_string();
    }

    trimmed[..tokens[end].1].trim_end().to_string()
}

/// Extract the numeric value from a trailing `LIMIT` clause, if present.
fn extract_user_limit(query: &str) -> Option<u32> {
    let trimmed = query.trim_end().trim_end_matches(';').trim_end();
    let tokens = tokenize_with_pos(trimmed);
    let len = tokens.len();

    let mut end = len;
    if end >= 2
        && tokens[end - 2].0.to_uppercase() == "OFFSET"
        && tokens[end - 1].0.parse::<u64>().is_ok()
    {
        end -= 2;
    }

    if end >= 2 && tokens[end - 2].0.to_uppercase() == "LIMIT" {
        return tokens[end - 1].0.parse().ok();
    }

    None
}

/// Extract the numeric value from a trailing `OFFSET` clause, if present.
fn extract_user_offset(query: &str) -> Option<u32> {
    let trimmed = query.trim_end().trim_end_matches(';').trim_end();
    let tokens = tokenize_with_pos(trimmed);
    let end = tokens.len();

    if end >= 2 && tokens[end - 2].0.to_uppercase() == "OFFSET" {
        return tokens[end - 1].0.parse().ok();
    }

    None
}

/// Build a paginated query: strip any user-supplied LIMIT/OFFSET and append
/// this page's clause. A user LIMIT caps the total rows returned across all
/// pages; a user OFFSET is added to the per-page offset.
pub fn build_paginated_query(query: &str, page_size: u32, page: u32) -> String {
    let page_offset = limit_offset(page, page_size).1;
    let user_limit = extract_user_limit(query);
    let user_offset = extract_user_offset(query).unwrap_or(0);
    let base = strip_limit_offset(query);

    let fetch_count = match user_limit {
        Some(ul) => {
            let remaining = ul.saturating_sub(page_offset);
            remaining.min(page_size + 1)
        }
        None => page_size + 1,
    };

    let offset = user_offset.saturating_add(page_offset);

    format!("{} LIMIT {} OFFSET {}", base, fetch_count, offset)
}
