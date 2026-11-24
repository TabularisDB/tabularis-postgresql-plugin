//! Streaming splitter for SQL script files (`import_database_sql`).
//!
//! Splits on `;` outside of single-quoted strings (including `E'..'` backslash
//! escapes), quoted identifiers, dollar-quoted bodies, (nested) comments,
//! parentheses and `CREATE FUNCTION|PROCEDURE ... BEGIN ATOMIC ... END` bodies
//! (`CASE ... END` inside one is tracked).
//! A line that starts with a backslash before any statement text (a psql
//! meta-command such as `\connect`) is returned as a statement of its own, so
//! the caller can reject or skip it at a precise statement number instead of it
//! being glued onto the next statement. Deciding what to do with it is the
//! caller's job: this module only splits.
//!
//! Plain `'..'` strings treat backslash literally, which is PostgreSQL's
//! default (`standard_conforming_strings = on`) and what `pg_dump` emits.

use std::io::{BufRead, Read};
use std::sync::Arc;

/// PostgreSQL itself refuses a query string over 1 GiB, so a statement beyond
/// this can never run; failing here also bounds memory when an unterminated
/// quote would otherwise swallow the rest of a huge file.
pub const MAX_STATEMENT_BYTES: usize = 1 << 30;
/// What a single line may hold beyond the statement cap (the `;`, whitespace
/// and the next statement's start share a line with it).
const LINE_SLACK_BYTES: usize = 1 << 20;

/// One statement from a script: its text (no terminating `;`) and the
/// 1-based line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlStatement {
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lex {
    Normal,
    LineComment,
    BlockComment(usize),
    SingleQuote { backslash_escapes: bool },
    DoubleQuote,
    Dollar(Arc<[char]>),
}

pub struct StatementSplitter<R: BufRead> {
    reader: R,
    chars: Vec<char>,
    pos: usize,
    line_no: usize,
    state: Lex,
    construct_line: usize,
    text: String,
    started: bool,
    start_line: usize,
    done: bool,
    word: String,
    head: Vec<String>,
    paren_depth: usize,
    begin_depth: usize,
    pending_begin: bool,
    max_statement_bytes: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl<R: BufRead> StatementSplitter<R> {
    pub fn new(reader: R) -> Self {
        Self::with_max_statement_bytes(reader, MAX_STATEMENT_BYTES)
    }

    pub fn with_max_statement_bytes(reader: R, max_statement_bytes: usize) -> Self {
        Self {
            reader,
            chars: Vec::new(),
            pos: 0,
            line_no: 0,
            state: Lex::Normal,
            construct_line: 0,
            text: String::new(),
            started: false,
            start_line: 0,
            done: false,
            word: String::new(),
            head: Vec::new(),
            paren_depth: 0,
            begin_depth: 0,
            pending_begin: false,
            max_statement_bytes,
        }
    }

    pub fn next_statement(&mut self) -> Result<Option<SqlStatement>, String> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.pos >= self.chars.len() {
                if !self.read_line()? {
                    self.done = true;
                    return self.finish();
                }
                continue;
            }
            if self.text.len() > self.max_statement_bytes {
                self.done = true;
                return Err(format!(
                    "Statement starting at line {} exceeds the maximum size of {} bytes",
                    self.start_line, self.max_statement_bytes
                ));
            }
            if let Some(statement) = self.step() {
                return Ok(Some(statement));
            }
        }
    }

    fn read_line(&mut self) -> Result<bool, String> {
        let mut buf = Vec::new();
        // A line is held whole (and expanded to `chars`, four bytes a
        // character), so bound it: one without a newline in sight stops here
        // instead of exhausting memory before the statement-size check runs.
        let limit = self.max_statement_bytes.saturating_add(LINE_SLACK_BYTES) as u64;
        let read = self
            .reader
            .by_ref()
            .take(limit)
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("Failed to read SQL file: {e}"))?;
        if read == 0 {
            return Ok(false);
        }
        if buf.len() as u64 >= limit && buf.last() != Some(&b'\n') {
            self.done = true;
            return Err(format!(
                "Line {} exceeds the maximum statement size of {} bytes",
                self.line_no + 1,
                self.max_statement_bytes
            ));
        }
        self.line_no += 1;
        let line = String::from_utf8(buf)
            .map_err(|_| format!("SQL file is not valid UTF-8 (line {})", self.line_no))?;
        let line = if self.line_no == 1 {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        } else {
            &line
        };
        self.chars = line.chars().collect();
        self.pos = 0;
        Ok(true)
    }

    fn finish(&mut self) -> Result<Option<SqlStatement>, String> {
        let what = match self.state {
            Lex::Normal | Lex::LineComment => None,
            Lex::BlockComment(_) => Some("block comment"),
            Lex::SingleQuote { .. } => Some("string literal"),
            Lex::DoubleQuote => Some("quoted identifier"),
            Lex::Dollar(_) => Some("dollar-quoted string"),
        };
        if let Some(what) = what {
            return Err(format!(
                "Unterminated {what} starting at line {}",
                self.construct_line
            ));
        }
        self.finish_word();
        Ok(self.emit())
    }

    fn emit(&mut self) -> Option<SqlStatement> {
        self.word.clear();
        self.head.clear();
        self.paren_depth = 0;
        self.begin_depth = 0;
        self.pending_begin = false;
        let started = std::mem::replace(&mut self.started, false);
        let text = std::mem::take(&mut self.text);
        let text = text.trim();
        if !started || text.is_empty() {
            return None;
        }
        Some(SqlStatement {
            text: text.to_string(),
            line: self.start_line,
        })
    }

    fn begin(&mut self) {
        if !self.started {
            self.started = true;
            self.start_line = self.line_no;
        }
    }

    fn keep(&mut self, c: char) {
        if self.started {
            self.text.push(c);
        }
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Consume at least one character; returns a statement when one completes.
    fn step(&mut self) -> Option<SqlStatement> {
        let c = self.chars[self.pos];
        match self.state.clone() {
            Lex::Normal => self.step_normal(c),
            Lex::LineComment => {
                self.keep(c);
                self.pos += 1;
                if c == '\n' || c == '\r' {
                    self.state = Lex::Normal;
                }
                None
            }
            Lex::BlockComment(depth) => {
                if c == '*' && self.peek(1) == Some('/') {
                    self.keep('*');
                    self.keep('/');
                    self.pos += 2;
                    self.state = if depth == 1 {
                        Lex::Normal
                    } else {
                        Lex::BlockComment(depth - 1)
                    };
                } else if c == '/' && self.peek(1) == Some('*') {
                    self.keep('/');
                    self.keep('*');
                    self.pos += 2;
                    self.state = Lex::BlockComment(depth + 1);
                } else {
                    self.keep(c);
                    self.pos += 1;
                }
                None
            }
            Lex::SingleQuote { backslash_escapes } => {
                self.text.push(c);
                self.pos += 1;
                if backslash_escapes && c == '\\' {
                    if let Some(next) = self.peek(0) {
                        self.text.push(next);
                        self.pos += 1;
                    }
                } else if c == '\'' {
                    if self.peek(0) == Some('\'') {
                        self.text.push('\'');
                        self.pos += 1;
                    } else {
                        self.state = Lex::Normal;
                    }
                }
                None
            }
            Lex::DoubleQuote => {
                self.text.push(c);
                self.pos += 1;
                if c == '"' {
                    if self.peek(0) == Some('"') {
                        self.text.push('"');
                        self.pos += 1;
                    } else {
                        self.state = Lex::Normal;
                    }
                }
                None
            }
            Lex::Dollar(tag) => {
                if c == '$' && self.chars[self.pos..].starts_with(&tag) {
                    self.text.extend(tag.iter());
                    self.pos += tag.len();
                    self.state = Lex::Normal;
                } else {
                    self.text.push(c);
                    self.pos += 1;
                }
                None
            }
        }
    }

    /// psql's heuristic for `CREATE [OR REPLACE] FUNCTION|PROCEDURE ... BEGIN
    /// ATOMIC ... END`: bare semicolons inside the body don't end the
    /// statement. `CASE` nests an `END` that must not close the body early.
    fn finish_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word).to_ascii_lowercase();
        if self.head.len() < 5 {
            self.head.push(word.clone());
        }
        let h = &self.head;
        let routine_len = if h.first().map(String::as_str) != Some("create") {
            None
        } else if matches!(h.get(1).map(String::as_str), Some("function" | "procedure")) {
            Some(2)
        } else if h.get(1).map(String::as_str) == Some("or")
            && h.get(2).map(String::as_str) == Some("replace")
            && matches!(h.get(3).map(String::as_str), Some("function" | "procedure"))
        {
            Some(4)
        } else {
            None
        };
        let Some(header) = routine_len else { return };
        if self.head.len() <= header {
            return;
        }
        // Only `BEGIN ATOMIC` opens a body: a bare `begin` can be a function
        // or parameter name, and treating it as a body would swallow every
        // following statement (including a COMMIT) into this one.
        if std::mem::take(&mut self.pending_begin) && word == "atomic" {
            self.begin_depth += 1;
            return;
        }
        match word.as_str() {
            "begin" if self.paren_depth == 0 && self.begin_depth == 0 => self.pending_begin = true,
            "case" if self.begin_depth > 0 => self.begin_depth += 1,
            "end" if self.begin_depth > 0 => self.begin_depth -= 1,
            _ => {}
        }
    }

    fn step_normal(&mut self, c: char) -> Option<SqlStatement> {
        if !(c.is_alphanumeric() || c == '_') {
            self.finish_word();
            if !c.is_whitespace() && !matches!(c, '-' | '/') {
                self.pending_begin = false;
            }
        }
        match c {
            ';' if self.paren_depth == 0 && self.begin_depth == 0 => {
                self.pos += 1;
                self.emit()
            }
            '(' => {
                self.begin();
                self.paren_depth += 1;
                self.text.push(c);
                self.pos += 1;
                None
            }
            ')' => {
                self.begin();
                self.paren_depth = self.paren_depth.saturating_sub(1);
                self.text.push(c);
                self.pos += 1;
                None
            }
            '-' if self.peek(1) == Some('-') => {
                self.construct_line = self.line_no;
                self.keep('-');
                self.keep('-');
                self.pos += 2;
                self.state = Lex::LineComment;
                None
            }
            '/' if self.peek(1) == Some('*') => {
                self.construct_line = self.line_no;
                self.keep('/');
                self.keep('*');
                self.pos += 2;
                self.state = Lex::BlockComment(1);
                None
            }
            '\\' if !self.started => {
                self.begin();
                let rest: String = self.chars[self.pos..].iter().collect();
                self.pos = self.chars.len();
                self.text = rest;
                self.emit()
            }
            '\'' => {
                self.begin();
                let mut prev = self.text.chars().rev();
                let backslash_escapes = matches!(prev.next(), Some('e' | 'E'))
                    && !prev.next().is_some_and(is_ident_char);
                self.construct_line = self.line_no;
                self.text.push(c);
                self.pos += 1;
                self.state = Lex::SingleQuote { backslash_escapes };
                None
            }
            '"' => {
                self.begin();
                self.construct_line = self.line_no;
                self.text.push(c);
                self.pos += 1;
                self.state = Lex::DoubleQuote;
                None
            }
            '$' => {
                self.begin();
                let after_ident = self.text.chars().next_back().is_some_and(is_ident_char);
                match (!after_ident).then(|| self.dollar_tag()).flatten() {
                    Some(tag) => {
                        self.construct_line = self.line_no;
                        self.text.extend(tag.iter());
                        self.pos += tag.len();
                        self.state = Lex::Dollar(tag.into());
                    }
                    None => {
                        self.text.push(c);
                        self.pos += 1;
                    }
                }
                None
            }
            c if c.is_whitespace() => {
                self.keep(c);
                self.pos += 1;
                None
            }
            c => {
                self.begin();
                if c.is_alphanumeric() || c == '_' {
                    self.word.push(c);
                }
                self.text.push(c);
                self.pos += 1;
                None
            }
        }
    }

    /// The opening `$tag$` at the cursor, or `None` when the `$` is a
    /// parameter marker (`$1`) or otherwise not a dollar-quote opener.
    fn dollar_tag(&self) -> Option<Vec<char>> {
        let mut end = 1;
        match self.peek(1)? {
            '$' => {}
            c if c.is_alphabetic() || c == '_' => {
                while self
                    .peek(end)
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
                {
                    end += 1;
                }
                if self.peek(end) != Some('$') {
                    return None;
                }
            }
            _ => return None,
        }
        Some(self.chars[self.pos..=self.pos + end].to_vec())
    }
}

#[cfg(test)]
#[path = "sql_split_tests.rs"]
mod sql_split_tests;
