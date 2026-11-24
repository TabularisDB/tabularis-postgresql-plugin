//! Streaming splitter for SQL script files (`import_database_sql`).
//!
//! Splits on `;` outside of single-quoted strings (including `E'..'` backslash
//! escapes), quoted identifiers, dollar-quoted bodies and (nested) comments.
//! Lines that start with a backslash before any statement text (psql
//! meta-commands such as `\connect`) are returned as statements of their own so
//! the server rejects them at a precise statement number instead of them being
//! glued onto the next statement.
//!
//! Plain `'..'` strings treat backslash literally, which is PostgreSQL's
//! default (`standard_conforming_strings = on`) and what `pg_dump` emits.

use std::io::BufRead;

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
    Dollar(Vec<char>),
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
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl<R: BufRead> StatementSplitter<R> {
    pub fn new(reader: R) -> Self {
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
            if let Some(statement) = self.step() {
                return Ok(Some(statement));
            }
        }
    }

    fn read_line(&mut self) -> Result<bool, String> {
        let mut buf = Vec::new();
        let read = self
            .reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("Failed to read SQL file: {e}"))?;
        if read == 0 {
            return Ok(false);
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
                if c == '\n' {
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
        match word.as_str() {
            "begin" => self.begin_depth += 1,
            "case" if self.begin_depth > 0 => self.begin_depth += 1,
            "end" if self.begin_depth > 0 => self.begin_depth -= 1,
            _ => {}
        }
    }

    fn step_normal(&mut self, c: char) -> Option<SqlStatement> {
        if !(c.is_alphanumeric() || c == '_') {
            self.finish_word();
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
                        self.state = Lex::Dollar(tag);
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
