//! Pagination math for LIMIT/OFFSET queries.
//!
//! `build_paginated_query` mirrors the builtin driver's behavior in
//! `src-tauri/src/drivers/common/query.rs`: strip any trailing user-supplied
//! `LIMIT`/`OFFSET`, honor the user's LIMIT as a cap across pages, and append
//! the plugin's own pagination clause. ORDER BY is left in place (not wrapped
//! in a subquery) so table-qualified column references stay valid.
//!
//! The query is read by a small lexer that knows `--` and (nested) `/* */`
//! comments, `'..'` and `E'..'` strings, `".."` identifiers and dollar-quoted
//! bodies. One scan drives the strip, both extractions and the append, so a
//! trailing comment never hides the user's own `LIMIT`/`OFFSET` and the paging
//! clause is never appended inside a `--` comment (where PostgreSQL would
//! ignore it and every page would return the same rows).
//!
//! A plain `'..'` string is read with a literal backslash, which is
//! PostgreSQL's default (`standard_conforming_strings = on`). With it off, a
//! `\'` inside a plain string would be misread.

const MAX_OFFSET: u64 = i64::MAX as u64;

/// Compute the SQL LIMIT and OFFSET for a given page and page size.
/// Pages are 1-indexed.
///
/// Widened to `u64`: a caller may send the largest `u32` for both, and their
/// product (and `page_size + 1`) does not fit in 32 bits. PostgreSQL's
/// `OFFSET` is a `bigint`, so the offset is capped at `i64::MAX` (the largest
/// pair of `u32`s multiplies past it); past the end of any table it simply
/// returns no rows instead of failing with "bigint out of range".
pub fn limit_offset(page: u32, page_size: u32) -> (u64, u64) {
    let offset = (u64::from(page.saturating_sub(1)) * u64::from(page_size)).min(MAX_OFFSET);
    (u64::from(page_size), offset)
}

/// What a lexed piece of the query is. Whitespace is skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A run of letters, digits and `_`: a keyword, an unquoted name or a number.
    Word,
    Semicolon,
    LineComment,
    BlockComment,
    /// Punctuation, a string, a quoted identifier or a dollar-quoted body.
    Other,
}

struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Splits `sql` into tokens with their byte spans. Anything unterminated (a
/// string, a quoted identifier, a block comment) runs to the end of the text.
fn lex(sql: &str) -> Vec<Tok> {
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|&(_, c)| c);
    let offset = |i: usize| chars.get(i).map_or(sql.len(), |&(o, _)| o);
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (start_offset, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let kind = if c == '-' && at(i + 1) == Some('-') {
            // PostgreSQL ends a `--` comment at `\n` or `\r`.
            while at(i).is_some_and(|c| c != '\n' && c != '\r') {
                i += 1;
            }
            Kind::LineComment
        } else if c == '/' && at(i + 1) == Some('*') {
            let mut depth = 1;
            i += 2;
            while i < chars.len() && depth > 0 {
                match (chars[i].1, at(i + 1)) {
                    ('*', Some('/')) => {
                        depth -= 1;
                        i += 2;
                    }
                    ('/', Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            i = i.min(chars.len());
            Kind::BlockComment
        } else if is_word_char(c) {
            while at(i).is_some_and(is_word_char) {
                i += 1;
            }
            Kind::Word
        } else if c == '\'' || c == '"' {
            // `E'..'` (an `e`/`E` that is not the tail of a longer name) lets a
            // backslash escape the next character.
            let escapes = c == '\''
                && start > 0
                && matches!(chars[start - 1].1, 'e' | 'E')
                && !(start > 1 && is_word_char(chars[start - 2].1));
            i += 1;
            while i < chars.len() {
                if escapes && chars[i].1 == '\\' {
                    i += 2;
                } else if chars[i].1 == c {
                    if at(i + 1) == Some(c) {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            i = i.min(chars.len());
            Kind::Other
        } else if c == '$'
            && !(start > 0 && (is_word_char(chars[start - 1].1) || chars[start - 1].1 == '$'))
        {
            let mut end = i + 1;
            while at(end).is_some_and(is_word_char) {
                end += 1;
            }
            let tag_ok = at(end) == Some('$') && !at(i + 1).is_some_and(|c| c.is_ascii_digit());
            if tag_ok {
                let tag: Vec<char> = chars[i..=end].iter().map(|&(_, c)| c).collect();
                i = end + 1;
                while i < chars.len()
                    && !chars[i..]
                        .iter()
                        .map(|&(_, c)| c)
                        .take(tag.len())
                        .eq(tag.iter().copied())
                {
                    i += 1;
                }
                i = (i + tag.len()).min(chars.len());
            } else {
                i += 1;
            }
            Kind::Other
        } else if c == ';' {
            i += 1;
            Kind::Semicolon
        } else {
            i += 1;
            Kind::Other
        };
        toks.push(Tok {
            kind,
            start: start_offset,
            end: offset(i),
        });
    }
    toks
}

/// A query split into the part to page, the user's own trailing `LIMIT` and
/// `OFFSET`, and the comments that followed the code.
struct Parsed<'a> {
    base: &'a str,
    /// What goes between `base` and the page clause: a newline when `base` ends
    /// inside a `--` comment (which would otherwise swallow the clause).
    separator: &'static str,
    user_limit: Option<u64>,
    user_offset: Option<u64>,
    trailing_comments: String,
}

fn parse(sql: &str) -> Parsed<'_> {
    let toks = lex(sql);
    let code: Vec<&Tok> = toks
        .iter()
        .filter(|t| !matches!(t.kind, Kind::LineComment | Kind::BlockComment))
        .collect();
    let mut end = code.len();
    while end > 0 && code[end - 1].kind == Kind::Semicolon {
        end -= 1;
    }
    let text = |t: &Tok| &sql[t.start..t.end];
    // `<keyword> <number>` as the last two tokens of `code[..upto]`.
    let clause = |upto: usize, keyword: &str| -> Option<u64> {
        if upto < 2 {
            return None;
        }
        let (word, number) = (code[upto - 2], code[upto - 1]);
        if word.kind == Kind::Word
            && number.kind == Kind::Word
            && text(word).eq_ignore_ascii_case(keyword)
        {
            text(number).parse().ok()
        } else {
            None
        }
    };

    let mut cut = end;
    let user_offset = clause(cut, "OFFSET");
    if user_offset.is_some() {
        cut -= 2;
    }
    let user_limit = clause(cut, "LIMIT");
    if user_limit.is_some() {
        cut -= 2;
    }

    let code_end = end.checked_sub(1).map_or(0, |i| code[i].end);
    let base_end = if cut == end {
        code_end
    } else {
        code[cut].start
    };
    let trailing_comments = toks
        .iter()
        .filter(|t| matches!(t.kind, Kind::LineComment | Kind::BlockComment) && t.start >= code_end)
        .map(text)
        .collect::<Vec<_>>()
        .join("\n");
    let base = sql[..base_end].trim_end();
    let ends_in_line_comment = toks
        .iter()
        .rfind(|t| t.end <= base.len())
        .is_some_and(|t| t.kind == Kind::LineComment);
    Parsed {
        base,
        separator: if ends_in_line_comment { "\n" } else { " " },
        user_limit,
        user_offset,
        trailing_comments,
    }
}

/// Build a paginated query: strip any user-supplied LIMIT/OFFSET and append
/// this page's clause. A user LIMIT caps the total rows returned across all
/// pages; a user OFFSET is added to the per-page offset. Comments after the
/// code are kept, after the clause.
pub fn build_paginated_query(query: &str, page_size: u32, page: u32) -> String {
    let (page_size, page_offset) = limit_offset(page, page_size);
    let Parsed {
        base,
        separator,
        user_limit,
        user_offset,
        trailing_comments,
    } = parse(query);

    let fetch_count = match user_limit {
        Some(ul) => {
            let remaining = ul.saturating_sub(page_offset);
            remaining.min(page_size + 1)
        }
        None => page_size + 1,
    };

    let offset = user_offset
        .unwrap_or(0)
        .saturating_add(page_offset)
        .min(MAX_OFFSET);

    let comments = if trailing_comments.is_empty() {
        String::new()
    } else {
        format!(" {trailing_comments}")
    };
    format!("{base}{separator}LIMIT {fetch_count} OFFSET {offset}{comments}")
}
