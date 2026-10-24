// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The parser: shell text to the syntax tree of [`crate::ast`].
//!
//! It reads characters, not tokens, as dash and bash do: how a character
//! is read depends on where it is (a `)` ends a subshell but not a case
//! pattern's word inside `$(...)`, `#` starts a comment only at the start
//! of a word, a reserved word is one only where a command starts), so the
//! grammar's functions read the text themselves. Aliases are put into the
//! text where their name was, and read again.
//!
//! Text that ends inside a construct (a quote, `$(`, an `if` with no
//! `fi`, a line that ends with `|`, a here-document with no delimiter
//! line) is [`Error::Incomplete`], so that the prompt can read another
//! line and parse again; any other mistake is [`Error::Syntax`], with the
//! line it is on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;

/// What went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The text ended inside a construct: more of it may complete it.
    Incomplete(Unfinished),
    /// The text is not shell: bash's messages, each with the line it gives
    /// it at, and for a token the grammar does not take where it is, the
    /// line it is on, which bash shows after them, at the last one's line,
    /// when not interactive.
    Syntax { messages: Vec<(u32, String)>, context: Option<String> },
}

/// What text that ended was inside, for bash's message at its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unfinished {
    /// A command or a list of them, or a compound command.
    Command,
    /// What bash looks for the `close` of: a quote, `${`, `((`, an array's
    /// `(`, from the line it opened on; a substitution read as commands
    /// (`$(`, `<(`), at the end.
    Matching { close: char, line: Option<u32> },
    /// A `[[` that opened on `line`, inside parentheses that opened on
    /// `parens`, outermost first: where a term should come (`term`), or
    /// after a whole expression.
    Cond { line: u32, parens: Vec<u32>, term: bool },
    /// A group in the regular expression after `=~`, from its line.
    Regex(u32),
    /// The bodies of here-documents, the first with this delimiter: at the
    /// real end of the text, they end there (see [`Parser::warnings`]).
    HereDoc(String),
}

const END_OF_FILE: &str = "syntax error: unexpected end of file";

impl Unfinished {
    /// bash's messages, each with the line it gives it at, when the text
    /// ends on `end`.
    pub fn report(&self, end: u32) -> Vec<(u32, String)> {
        let expected = |line: &u32| (*line, "expected `)'".to_string());
        match self {
            Unfinished::Command | Unfinished::HereDoc(_) => vec![(end, END_OF_FILE.into())],
            Unfinished::Matching { close, line } => vec![(
                line.unwrap_or(end),
                format!("unexpected EOF while looking for matching `{close}'"),
            )],
            Unfinished::Cond { parens, term: true, .. } => {
                let mut messages =
                    vec![(end, "unexpected token `EOF' in conditional command".to_string())];
                messages.extend(parens.iter().rev().map(expected));
                messages.push((end, END_OF_FILE.into()));
                messages
            }
            Unfinished::Cond { line, parens, term: false } => {
                let mut messages = match parens.split_last() {
                    None => vec![(*line, "unexpected EOF while looking for `]]'".to_string())],
                    Some((inner, outer)) => {
                        let mut messages =
                            vec![(*inner, "unexpected token `EOF', expected `)'".to_string())];
                        messages.extend(outer.iter().rev().map(expected));
                        messages
                    }
                };
                messages.push((end, END_OF_FILE.into()));
                messages
            }
            Unfinished::Regex(line) => vec![
                (*line, "unexpected EOF while looking for matching `)'".into()),
                (end, "unexpected argument to conditional binary operator".into()),
            ],
        }
    }
}

impl Error {
    /// The message, as for text that ends where this one did.
    pub fn message(&self) -> String {
        let messages = match self {
            Error::Incomplete(unfinished) => unfinished.report(0),
            Error::Syntax { messages, .. } => messages.clone(),
        };
        messages.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join("\n")
    }

    /// A syntax error made of one message.
    fn syntax(line: u32, message: impl Into<String>, context: Option<String>) -> Error {
        Error::Syntax { messages: vec![(line, message.into())], context }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// How to parse.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// POSIX mode: the extensions that change what POSIX text means
    /// (`&>`, `<(`, `function`, `[[`) are off.
    pub posix: bool,
    /// `shopt -s extglob`: `@(...)` and the other extended patterns.
    pub extglob: bool,
    /// The aliases, when they are expanded (interactive shells, and
    /// scripts after `shopt -s expand_aliases`).
    pub aliases: Option<Rc<HashMap<String, String>>>,
}

/// A here-document whose body comes after the line its `<<` is on.
struct Pending {
    doc: Rc<RefCell<HereDoc>>,
    delimiter: String,
    strip_tabs: bool,
    /// The line of its `<<`.
    line: u32,
}

/// Where a command list ends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    /// The end of the text.
    Eof,
    /// A `)`: a subshell, `$(...)` or a process substitution.
    Paren,
    /// One of these reserved words, or a case arm's `;;` and its kin.
    Words(&'static [&'static str]),
}

/// How [`Parser::word`] reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// An ordinary word.
    Normal,
    /// A word of `[[ ]]`: `<` and `>` end it, as `(` and `)` do.
    Cond,
    /// The right of `=~`: `(`, `)` and `|` are part of the regular
    /// expression.
    Regex,
    /// A command's first word, where `name[` opens a subscript that runs
    /// to its `]` whatever it holds, blanks and operators too, as bash
    /// reads `a[1 + 1]=x`.
    Command,
}

pub struct Parser {
    text: Vec<char>,
    pos: usize,
    line: u32,
    file: Rc<str>,
    options: Options,
    pending: Vec<Pending>,
    /// Aliases being expanded, each with the position its text ends at,
    /// so that one does not expand itself.
    expanding: Vec<(usize, String)>,
    /// The last alias's text ended with a blank: the next word is looked
    /// up as an alias too.
    alias_next: bool,
    /// Where the command being read started, and the text from there as
    /// it was before an alias was put into it: what [`Parser::next_command`]
    /// goes back to when the command is incomplete.
    start: usize,
    unspliced: Option<Vec<char>>,
    /// More text may come after what the parser holds, from
    /// [`Parser::push_text`] until [`Parser::end_text`]: a command whose
    /// text ends in a line continuation goes on in it.
    more: bool,
    /// Taking out a line continuation reached the end of the text.
    continued: bool,
    /// How deep the construct being read is nested (commands in
    /// commands, `$(...)` and `${...}` in words, `[[`'s `(` and `!`), up to
    /// [`MAX_DEPTH`].
    depth: u32,
    /// bash's warnings, with the lines they are given at, for
    /// [`Parser::warnings`].
    warnings: Vec<(u32, String)>,
    /// The line of the `[[` being read, and of the parentheses open in it.
    cond_line: u32,
    cond_parens: Vec<u32>,
    /// The line the last command read ends on.
    end: u32,
}

/// How deep constructs may nest. Deeper text is a syntax error, not a
/// stack overflow: each level costs the parser, and then the code that
/// runs, shows and drops what it read, a few kilobytes of stack.
pub const MAX_DEPTH: u32 = 1000;

/// The characters that end an unquoted word.
fn is_meta(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_name_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Whether `s` is a name: a letter or `_`, then letters, digits and `_`.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(is_name_start) && chars.all(is_name_char)
}

/// Whether `s` names a variable to set, as `printf -v` and `read` take
/// one: a name, or an element `name[subscript]` whose subscript is not
/// empty and whose `]` ends it. Brackets in the subscript nest; quotes
/// and backslashes in it hide them.
pub fn is_reference(s: &str) -> bool {
    let Some(open) = s.find('[') else { return is_name(s) };
    if !is_name(&s[..open]) {
        return false;
    }
    let subscript = &s.as_bytes()[open + 1..];
    let mut depth = 1;
    let mut i = 0;
    while i < subscript.len() {
        match subscript[i] {
            b'\\' => i += 1,
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < subscript.len() && subscript[i] != quote {
                    if quote == b'"' && subscript[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return i > 0 && i + 1 == subscript.len();
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// The commands that take `name=(...)` arrays as arguments.
const DECLARATIONS: [&str; 5] = ["declare", "typeset", "local", "export", "readonly"];

/// Reserved words, which are not commands where a command starts.
const RESERVED: [&str; 22] = [
    "!", "{", "}", "case", "do", "done", "elif", "else", "esac", "fi", "for", "if", "in", "then",
    "until", "while", "[[", "]]", "function", "select", "time", "coproc",
];

/// Whether `word` is a reserved word where a command starts.
pub fn is_reserved(word: &str) -> bool {
    RESERVED.contains(&word)
}

impl Parser {
    pub fn new(text: &str, file: Rc<str>, first_line: u32, options: Options) -> Parser {
        Parser {
            text: text.chars().collect(),
            pos: 0,
            line: first_line,
            file,
            options,
            pending: Vec::new(),
            expanding: Vec::new(),
            alias_next: false,
            start: 0,
            unspliced: None,
            more: false,
            continued: false,
            depth: 0,
            warnings: Vec::new(),
            cond_line: 0,
            cond_parens: Vec::new(),
            end: first_line,
        }
    }

    /// Reads with `read` one level deeper, or fails past [`MAX_DEPTH`].
    fn nested<T>(&mut self, read: impl FnOnce(&mut Parser) -> Result<T>) -> Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(self.syntax("too deeply nested"));
        }
        self.depth += 1;
        let result = read(self);
        self.depth -= 1;
        result
    }

    /// Adds text after what the parser holds: the next line typed, or read
    /// from a script that comes a line at a time.
    pub fn push_text(&mut self, text: &str) {
        self.text.extend(text.chars());
        self.more = true;
    }

    /// Takes back the text after the command just parsed, which its
    /// reader read ahead of it, so that it can be read again: `None` while
    /// aliases or here-documents still use it.
    pub fn take_rest(&mut self) -> Option<String> {
        if !self.pending.is_empty() || !self.expanding.is_empty() || self.pos == self.text.len() {
            return None;
        }
        let rest = self.text.drain(self.pos..).collect();
        self.more = true;
        Some(rest)
    }

    /// Says that no more text will come, so that a line continuation at
    /// the end of the text ends with it.
    pub fn end_text(&mut self) {
        self.more = false;
    }

    /// Changes how the rest of the text is parsed, as `alias`, `shopt -s
    /// extglob` and `set -o posix` do between commands.
    pub fn set_options(&mut self, options: Options) {
        self.options = options;
    }

    /// Forgets the text already parsed, so that a long session does not
    /// keep all it was given.
    pub fn compact(&mut self) {
        if self.pos > 0 && self.pending.is_empty() && self.expanding.is_empty() {
            self.text.drain(..self.pos);
            self.pos = 0;
        }
    }

    /// Parses all of the text.
    pub fn parse(mut self) -> Result<Program> {
        let program = self.list(End::Eof)?;
        self.skip_blanks();
        match self.peek() {
            None => {}
            Some(c) => return Err(self.unexpected(c)),
        }
        self.finish_heredocs()?;
        Ok(program)
    }

    /// Parses the next complete command and stops at the end of its line,
    /// for reading a script one command at a time: `None` at the end of the
    /// text. When the text ends inside the command, the parser goes back to
    /// where the command started, so that the caller can add the next line
    /// and call again. So does a command whose text, as far as the parser
    /// holds it, ends in a line continuation (`cmd \` then a newline),
    /// while more text may come: the next line is part of it.
    pub fn next_command(&mut self) -> Result<Option<CompleteCommand>> {
        let saved = (self.pos, self.line, self.alias_next);
        self.start = self.pos;
        self.unspliced = None;
        self.continued = false;
        let mut result = self.next_command_here();
        if self.more && self.continued && matches!(result, Ok(Some(_))) {
            result = Err(self.unfinished());
        }
        if let Err(Error::Incomplete(_)) = result {
            if let Some(text) = self.unspliced.take() {
                self.text.truncate(self.start);
                self.text.extend(text);
            }
            (self.pos, self.line, self.alias_next) = saved;
            self.pending.clear();
            self.expanding.clear();
        }
        result
    }

    fn next_command_here(&mut self) -> Result<Option<CompleteCommand>> {
        self.linebreak()?;
        if self.peek().is_none() {
            return Ok(None);
        }
        let mut command = CompleteCommand::default();
        loop {
            let item = self.and_or()?;
            let sep = self.separator();
            command.items.push((item, sep.unwrap_or(Sep::Seq)));
            self.skip_blanks();
            match self.peek() {
                None => {
                    self.end = self.line;
                    self.finish_heredocs()?;
                    break;
                }
                Some('\n') => {
                    self.end = self.line;
                    self.newline()?;
                    break;
                }
                Some(c) if command_ends(c) || sep.is_none() => return Err(self.unexpected(c)),
                Some(_) => {}
            }
        }
        Ok(Some(command))
    }

    /// The warnings bash gives for the text read so far, which a caller
    /// prints, each at its line: a here-document the end of the text
    /// ended.
    pub fn warnings(&mut self) -> Vec<(u32, String)> {
        std::mem::take(&mut self.warnings)
    }

    // Reading characters.

    /// The next character, with line continuations (a backslash before a
    /// newline) taken out.
    fn peek(&mut self) -> Option<char> {
        while self.text.get(self.pos) == Some(&'\\') && self.text.get(self.pos + 1) == Some(&'\n') {
            self.pos += 2;
            self.line += 1;
            self.continued |= self.pos == self.text.len();
        }
        self.leave_aliases();
        self.text.get(self.pos).copied()
    }

    /// The next character, as it is.
    fn peek_raw(&self) -> Option<char> {
        self.text.get(self.pos).copied()
    }

    /// The character after the next, line continuations taken out.
    fn peek2(&mut self) -> Option<char> {
        self.peek()?;
        let mut at = self.pos + 1;
        while self.text.get(at) == Some(&'\\') && self.text.get(at + 1) == Some(&'\n') {
            at += 2;
        }
        self.text.get(at).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn bump_raw(&mut self) -> Option<char> {
        let c = self.peek_raw()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Whether the text goes on with `s`, line continuations taken out.
    fn at(&mut self, s: &str) -> bool {
        if self.peek().is_none() {
            return s.is_empty();
        }
        let mut at = self.pos;
        for want in s.chars() {
            while self.text.get(at) == Some(&'\\') && self.text.get(at + 1) == Some(&'\n') {
                at += 2;
            }
            if self.text.get(at) != Some(&want) {
                return false;
            }
            at += 1;
        }
        true
    }

    /// Consumes `s` if the text goes on with it.
    fn eat(&mut self, s: &str) -> bool {
        if !self.at(s) {
            return false;
        }
        for _ in s.chars() {
            self.bump();
        }
        true
    }

    /// Whether the reserved word `word` is next: the word, then something
    /// that ends a word.
    fn at_word(&mut self, word: &str) -> bool {
        if !self.at(word) {
            return false;
        }
        let mut at = self.pos;
        for _ in word.chars() {
            while self.text.get(at) == Some(&'\\') && self.text.get(at + 1) == Some(&'\n') {
                at += 2;
            }
            at += 1;
        }
        match self.text.get(at) {
            None => true,
            Some(&c) => is_meta(c) || (word == "}" || word == "]]") && c == '}',
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.eat(word);
            true
        } else {
            false
        }
    }

    /// Skips blanks and a comment.
    fn skip_blanks(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' => self.pos += 1,
                '#' => {
                    while let Some(c) = self.peek_raw() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Consumes a newline, then reads the bodies of the here-documents
    /// its line started.
    fn newline(&mut self) -> Result<()> {
        self.bump();
        self.read_heredocs()
    }

    /// Skips blanks, comments and newlines.
    fn linebreak(&mut self) -> Result<()> {
        loop {
            self.skip_blanks();
            if self.peek() == Some('\n') {
                self.newline()?;
            } else {
                return Ok(());
            }
        }
    }

    fn syntax(&self, message: impl Into<String>) -> Error {
        Error::syntax(self.line, message, None)
    }

    /// A syntax error that bash shows the line of after it.
    fn syntax_here(&self, message: impl Into<String>) -> Error {
        Error::syntax(self.line, message, Some(self.this_line()))
    }

    /// The line the read position is on, as it was read.
    fn this_line(&self) -> String {
        self.line_at(self.pos)
    }

    /// The line `at` is on, as it was read; a newline at `at` ends it.
    fn line_at(&self, at: usize) -> String {
        let at = at.min(self.text.len());
        let start = self.text[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let end =
            self.text[at..].iter().position(|&c| c == '\n').map_or(self.text.len(), |i| at + i);
        self.text[start..end].iter().collect()
    }

    /// The line the command [`Parser::next_command`] read last ends on.
    pub fn command_end(&self) -> u32 {
        self.end
    }

    /// The line bash names for the end of the text: the one after its
    /// last, a last line without a newline counted.
    pub fn end_line(&self) -> u32 {
        let rest = &self.text[self.pos.min(self.text.len())..];
        let newlines = rest.iter().filter(|&&c| c == '\n').count();
        let partial = rest.last().is_some_and(|&c| c != '\n');
        self.line + u32::try_from(newlines).unwrap_or(u32::MAX) + u32::from(partial)
    }

    /// Text that ends inside a command.
    fn unfinished(&self) -> Error {
        Error::Incomplete(Unfinished::Command)
    }

    /// Text that ends before the `close` of what opened on `line`.
    fn unmatched(close: char, line: u32) -> Error {
        Error::Incomplete(Unfinished::Matching { close, line: Some(line) })
    }

    /// [`Parser::unmatched`], for what opened on the line the text ends
    /// on.
    fn unmatched_here(&self, close: char) -> Error {
        Parser::unmatched(close, self.line)
    }

    /// The text ends where a word must come: at its real end, bash, which
    /// ends text with a newline, finds the newline unexpected.
    fn ends_here(&mut self) -> Error {
        if self.more { self.unfinished() } else { self.unexpected('\n') }
    }

    /// The error for something the grammar does not allow here.
    fn unexpected(&mut self, c: char) -> Error {
        let token = self.token(c);
        self.syntax_here(format!("syntax error near unexpected token `{token}'"))
    }

    /// The token that starts with `c`, at the read position, as bash
    /// names it in an error.
    fn token(&self, c: char) -> String {
        if c == '\n' {
            "newline".to_string()
        } else {
            let mut token = String::new();
            let mut at = self.pos;
            while let Some(&c) = self.text.get(at) {
                if is_meta(c) && !token.is_empty() || c == '\n' {
                    break;
                }
                token.push(c);
                at += 1;
                if is_meta(c) {
                    // An operator: take what goes with it.
                    while let Some(&next) = self.text.get(at) {
                        if matches!(next, '&' | '|' | ';' | '<' | '>') && token.len() < 3 {
                            token.push(next);
                            at += 1;
                        } else {
                            break;
                        }
                    }
                    break;
                }
            }
            token
        }
    }

    // Aliases.

    /// Drops the aliases whose text has been read.
    fn leave_aliases(&mut self) {
        while let Some((end, _)) = self.expanding.last() {
            if self.pos >= *end {
                self.expanding.pop();
            } else {
                break;
            }
        }
    }

    /// If the next word is an alias, puts its text in its place; true if
    /// it did.
    fn expand_alias(&mut self) -> bool {
        let Some(aliases) = self.options.aliases.clone() else { return false };
        let mut expanded = false;
        loop {
            self.skip_blanks();
            let start = self.pos;
            let mut end = start;
            while let Some(&c) = self.text.get(end) {
                if is_meta(c) || matches!(c, '\'' | '"' | '\\' | '$' | '`' | '=') {
                    break;
                }
                end += 1;
            }
            if end == start {
                return expanded;
            }
            let ends_word = self.text.get(end).is_none_or(|&c| is_meta(c));
            if !ends_word {
                return expanded;
            }
            let name: String = self.text[start..end].iter().collect();
            if self.expanding.iter().any(|(_, active)| *active == name) {
                return expanded;
            }
            let Some(value) = aliases.get(&name) else { return expanded };
            let value: Vec<char> = value.chars().collect();
            if self.unspliced.is_none() && start >= self.start {
                self.unspliced = Some(self.text[self.start..].to_vec());
            }
            let grow = value.len() as isize - (end - start) as isize;
            self.text.splice(start..end, value.iter().copied());
            for (stop, _) in &mut self.expanding {
                *stop = (*stop as isize + grow) as usize;
            }
            let stop = start + value.len();
            self.expanding.push((stop, name));
            self.alias_next = value.last().is_some_and(|c| *c == ' ' || *c == '\t');
            expanded = true;
            // The alias's text may start with another alias.
            if self.pos >= stop {
                return true;
            }
        }
    }

    // The grammar.

    /// Complete commands until `end`, which is not consumed.
    fn list(&mut self, end: End) -> Result<Program> {
        let mut program = Program::default();
        loop {
            self.linebreak()?;
            if self.at_end(end) {
                return Ok(program);
            }
            let mut command = CompleteCommand::default();
            loop {
                let item = self.and_or()?;
                let sep = self.separator();
                command.items.push((item, sep.unwrap_or(Sep::Seq)));
                self.skip_blanks();
                match self.peek() {
                    None => break,
                    Some('\n') => break,
                    _ if self.at_end(end) => break,
                    Some(c) if command_ends(c) || sep.is_none() => return Err(self.unexpected(c)),
                    Some(_) => {}
                }
            }
            program.commands.push(command);
        }
    }

    /// The `;` or `&` after a command, if one is next. Only one lets
    /// another command follow on the line: a word after a compound
    /// command's end is an error.
    fn separator(&mut self) -> Option<Sep> {
        self.skip_blanks();
        match self.peek() {
            Some('&') if !self.at("&&") && !self.at("&>") => {
                self.pos += 1;
                Some(Sep::Async)
            }
            Some(';') if !self.at(";;") && !self.at(";&") => {
                self.pos += 1;
                Some(Sep::Seq)
            }
            _ => None,
        }
    }

    /// A list that must have a command: the body of a compound command.
    fn compound_list(&mut self, end: End) -> Result<Program> {
        let program = self.list(end)?;
        if program.commands.is_empty() {
            return Err(match self.peek() {
                None => self.unfinished(),
                Some(c) => self.unexpected(c),
            });
        }
        Ok(program)
    }

    /// Whether `end` is next; the end of the text ends every list, but is
    /// an error for all but [`End::Eof`].
    fn at_end(&mut self, end: End) -> bool {
        self.skip_blanks();
        match self.peek() {
            None => true,
            Some(')') => end == End::Paren || matches!(end, End::Words(_)),
            Some(';') if self.at(";;") || self.at(";&") => matches!(end, End::Words(_)),
            Some(_) => match end {
                End::Words(words) => words.iter().any(|w| self.at_word(w)),
                _ => false,
            },
        }
    }

    fn and_or(&mut self) -> Result<AndOr> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            self.skip_blanks();
            let op = if self.eat("&&") {
                AndOrOp::And
            } else if self.eat("||") {
                AndOrOp::Or
            } else {
                return Ok(AndOr { first, rest });
            };
            self.linebreak()?;
            if self.peek().is_none() {
                return Err(self.unfinished());
            }
            rest.push((op, self.pipeline()?));
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline> {
        self.skip_blanks();
        let line = self.line;
        // `time` and `!`, in either order, as bash takes them.
        let mut time = None;
        let mut bang = false;
        let mut prefixed = false;
        loop {
            if self.at_word("time") {
                self.eat("time");
                self.skip_blanks();
                let posix_format = self.at_word("-p");
                if posix_format {
                    self.eat("-p");
                }
                time.get_or_insert(posix_format);
            } else if self.at_word("!") {
                self.eat("!");
                bang = !bang;
            } else {
                break;
            }
            prefixed = true;
            self.skip_blanks();
        }
        // Either alone before a newline or a `;` times or negates nothing,
        // as in bash.
        if prefixed
            && match self.peek() {
                None | Some('\n') => true,
                Some(';') => !self.at(";;") && !self.at(";&"),
                Some(_) => false,
            }
        {
            let nothing = Command::Simple(SimpleCommand { line, ..SimpleCommand::default() });
            return Ok(Pipeline {
                bang,
                time,
                commands: vec![nothing],
                pipe_stderr: Vec::new(),
                line,
            });
        }
        let mut commands = vec![self.command()?];
        let mut pipe_stderr = Vec::new();
        loop {
            self.skip_blanks();
            if self.at("||") || !self.at("|") {
                break;
            }
            self.bump();
            let both = !self.options.posix && self.eat("&");
            pipe_stderr.push(both);
            self.linebreak()?;
            if self.peek().is_none() {
                return Err(self.unfinished());
            }
            commands.push(self.command()?);
        }
        Ok(Pipeline { bang, time, commands, pipe_stderr, line })
    }

    fn command(&mut self) -> Result<Command> {
        self.nested(Parser::command_here)
    }

    fn command_here(&mut self) -> Result<Command> {
        let mut aliased = false;
        if self.alias_next || self.options.aliases.is_some() {
            self.alias_next = false;
            aliased = self.expand_alias();
        }
        self.skip_blanks();
        let line = self.line;
        // A line that was only an alias of nothing (`alias nothing=''`)
        // runs nothing, as a blank line would.
        if aliased && matches!(self.peek(), None | Some('\n')) {
            return Ok(Command::Simple(SimpleCommand { line, ..SimpleCommand::default() }));
        }
        let compound = match self.peek() {
            None => return Err(self.unfinished()),
            Some('(') if self.at("((") && !self.options.posix => match self.arith_command()? {
                Some(compound) => compound,
                None => self.subshell()?,
            },
            Some('(') => self.subshell()?,
            Some(c) if command_ends(c) => return Err(self.unexpected(c)),
            Some(_) => {
                if self.eat_word("{") {
                    let body = self.compound_list(End::Words(&["}"]))?;
                    self.expect_word("}")?;
                    CompoundCommand::Brace(body)
                } else if self.eat_word("if") {
                    self.if_command()?
                } else if self.eat_word("while") {
                    let cond = self.compound_list(End::Words(&["do"]))?;
                    let body = self.do_group()?;
                    CompoundCommand::While { cond, body }
                } else if self.eat_word("until") {
                    let cond = self.compound_list(End::Words(&["do"]))?;
                    let body = self.do_group()?;
                    CompoundCommand::Until { cond, body }
                } else if self.eat_word("for") {
                    self.for_command(line)?
                } else if self.eat_word("case") {
                    self.case_command(line)?
                } else if !self.options.posix && self.eat_word("select") {
                    self.select_command(line)?
                } else if !self.options.posix && self.eat_word("[[") {
                    CompoundCommand::Cond(self.cond_command()?, line)
                } else if !self.options.posix && self.eat_word("function") {
                    return self.function_keyword(line);
                } else if self.eat_word("coproc") {
                    return self.coproc_command();
                } else {
                    for word in ["}", "do", "done", "elif", "else", "esac", "fi", "then", "]]", "!"]
                    {
                        if self.at_word(word) {
                            return Err(self.syntax_here(format!(
                                "syntax error near unexpected token `{word}'"
                            )));
                        }
                    }
                    return self.simple_command(line);
                }
            }
        };
        let redirects = self.redirects()?;
        Ok(Command::Compound(Box::new(compound), redirects))
    }

    /// Whether a compound command starts here.
    fn at_compound(&mut self) -> bool {
        self.at("(")
            || ["{", "if", "while", "until", "for", "case"].into_iter().any(|w| self.at_word(w))
            || !self.options.posix && (self.at_word("select") || self.at_word("[["))
    }

    /// `coproc command`, or `coproc NAME compound-command`. As in bash, a
    /// word that is not an assignment names the coprocess when a compound
    /// command follows it, and is checked when it runs; any other
    /// reserved word after it is out of place.
    fn coproc_command(&mut self) -> Result<Command> {
        self.skip_blanks();
        if self.at_word("!") {
            return Err(self.syntax_here("syntax error near unexpected token `!'"));
        }
        if matches!(self.peek(), None | Some('\n')) {
            return Err(self.syntax_here("syntax error near unexpected token `newline'"));
        }
        let mut name = String::from("COPROC");
        if !self.at_compound() {
            let start = (self.pos, self.line, self.warnings.len());
            let assignment = self.assignment(true)?.is_some();
            (self.pos, self.line) = (start.0, start.1);
            self.warnings.truncate(start.2);
            if !assignment && let Some(word) = self.word(Ctx::Normal)? {
                self.skip_blanks();
                if self.at_compound() {
                    name = word.unquoted_text();
                } else {
                    for reserved in RESERVED {
                        if reserved != "time" && self.at_word(reserved) {
                            return Err(self.syntax_here(format!(
                                "syntax error near unexpected token `{reserved}'"
                            )));
                        }
                    }
                    (self.pos, self.line) = (start.0, start.1);
                    self.warnings.truncate(start.2);
                }
            }
        }
        let command = self.command()?;
        Ok(Command::Coproc(Box::new(Coproc { name, command, line: self.line })))
    }

    fn expect_word(&mut self, word: &str) -> Result<()> {
        self.linebreak()?;
        if self.eat_word(word) {
            return Ok(());
        }
        match self.peek() {
            None => Err(self.unfinished()),
            Some(c) => Err(self.unexpected(c)),
        }
    }

    fn subshell(&mut self) -> Result<CompoundCommand> {
        self.bump();
        let body = self.compound_list(End::Paren)?;
        self.linebreak()?;
        if !self.eat(")") {
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        if body.commands.is_empty() {
            return Err(self.syntax_here("syntax error near unexpected token `)'"));
        }
        Ok(CompoundCommand::Subshell(body, self.line))
    }

    /// `(( expression ))`, or `None` (with nothing consumed) when the
    /// `((` turns out to open nested subshells.
    fn arith_command(&mut self) -> Result<Option<CompoundCommand>> {
        let (start, start_line) = (self.pos, self.line);
        let line = self.line;
        self.eat("((");
        match self.arith_text()? {
            Some(text) => {
                let word = self.sub_word(&text, line, WordMode::Arith)?;
                Ok(Some(CompoundCommand::Arith(word, line)))
            }
            None => {
                self.pos = start;
                self.line = start_line;
                Ok(None)
            }
        }
    }

    /// The text of an arithmetic expression after `((` or `$((`, through
    /// the `))` that closes it; `None` if a lone `)` closes it first, so
    /// that it was `( (` after all.
    fn arith_text(&mut self) -> Result<Option<String>> {
        let line = self.line;
        let mut text = String::new();
        let mut depth = 0usize;
        loop {
            let Some(c) = self.bump() else { return Err(Parser::unmatched(')', line)) };
            match c {
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                ')' => {
                    if self.peek() == Some(')') {
                        self.bump();
                        return Ok(Some(text));
                    }
                    return Ok(None);
                }
                '\'' | '"' | '`' => {
                    // Quotes are read as they are, for the expander.
                    text.push(c);
                    let close = c;
                    let quote_line = self.line;
                    loop {
                        let Some(q) = self.bump_raw() else {
                            return Err(Parser::unmatched(close, quote_line));
                        };
                        text.push(q);
                        if q == '\\' && close != '\'' {
                            if let Some(n) = self.bump_raw() {
                                text.push(n);
                            }
                        } else if q == close {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            text.push(c);
        }
    }

    fn if_command(&mut self) -> Result<CompoundCommand> {
        let mut arms = Vec::new();
        let mut otherwise = None;
        loop {
            let cond = self.compound_list(End::Words(&["then"]))?;
            self.expect_word("then")?;
            let body = self.compound_list(End::Words(&["elif", "else", "fi"]))?;
            arms.push((cond, body));
            self.linebreak()?;
            if self.eat_word("elif") {
                continue;
            }
            if self.eat_word("else") {
                otherwise = Some(self.compound_list(End::Words(&["fi"]))?);
            }
            self.expect_word("fi")?;
            return Ok(CompoundCommand::If { arms, otherwise });
        }
    }

    fn do_group(&mut self) -> Result<Program> {
        self.expect_word("do")?;
        let body = self.compound_list(End::Words(&["done"]))?;
        self.expect_word("done")?;
        Ok(body)
    }

    /// The name after `for` or `select`: in bash's grammar any word, which
    /// the loop checks when it runs; in POSIX mode, as in dash, a name.
    fn loop_name(&mut self) -> Result<String> {
        self.skip_blanks();
        let start = (self.pos, self.line);
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        if let Some(name) = word.as_literal().filter(|name| is_name(name)) {
            return Ok(name.to_string());
        }
        let text: String = self.text[start.0..self.pos].iter().collect();
        if self.options.posix {
            (self.pos, self.line) = start;
            return Err(self.syntax_here(format!("`{text}': not a valid identifier")));
        }
        Ok(text)
    }

    /// The `in words` of `for` and `select`, through the `;` or newline
    /// after them; `None` without `in`, which means `"$@"`.
    fn loop_words(&mut self) -> Result<Option<Vec<Word>>> {
        self.skip_blanks();
        // `for x; do`, and `for x do`.
        if self.eat(";") {
            return Ok(None);
        }
        self.linebreak()?;
        if !self.eat_word("in") {
            return Ok(None);
        }
        let mut words = Vec::new();
        loop {
            self.skip_blanks();
            match self.peek() {
                None => return Err(self.unfinished()),
                Some(';') => {
                    self.bump();
                    break;
                }
                Some('\n') => break,
                Some(c) if is_meta(c) => return Err(self.unexpected(c)),
                Some(_) => match self.word(Ctx::Normal)? {
                    Some(word) => words.push(word),
                    None => break,
                },
            }
        }
        Ok(Some(words))
    }

    fn for_command(&mut self, line: u32) -> Result<CompoundCommand> {
        self.skip_blanks();
        if !self.options.posix && self.at("((") {
            self.eat("((");
            let Some(text) = self.arith_text()? else {
                return Err(self.syntax("syntax error: arithmetic expression required"));
            };
            let parts = split_arith_for(&text).ok_or_else(|| Error::Syntax {
                messages: vec![
                    (self.line, "syntax error: arithmetic expression required".into()),
                    (self.line, format!("syntax error: `(({text}))'")),
                ],
                context: None,
            })?;
            let init = self.sub_word(&parts[0], line, WordMode::Arith)?;
            let cond = self.sub_word(&parts[1], line, WordMode::Arith)?;
            let step = self.sub_word(&parts[2], line, WordMode::Arith)?;
            self.skip_blanks();
            self.eat(";");
            let body = self.loop_body()?;
            return Ok(CompoundCommand::ArithFor { init, cond, step, body, line });
        }
        let var = self.loop_name()?;
        let words = self.loop_words()?;
        let body = self.loop_body()?;
        Ok(CompoundCommand::For { var, words, body, line })
    }

    /// `do list done`, or bash's `{ list }` for `for` and `select`.
    fn loop_body(&mut self) -> Result<Program> {
        self.linebreak()?;
        if !self.options.posix && self.eat_word("{") {
            let body = self.compound_list(End::Words(&["}"]))?;
            self.expect_word("}")?;
            return Ok(body);
        }
        self.do_group()
    }

    fn select_command(&mut self, line: u32) -> Result<CompoundCommand> {
        let var = self.loop_name()?;
        let words = self.loop_words()?;
        let body = self.loop_body()?;
        Ok(CompoundCommand::Select { var, words, body, line })
    }

    fn case_command(&mut self, line: u32) -> Result<CompoundCommand> {
        self.skip_blanks();
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        self.linebreak()?;
        if !self.eat_word("in") {
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        let mut arms = Vec::new();
        loop {
            self.linebreak()?;
            if self.eat_word("esac") {
                return Ok(CompoundCommand::Case { word, arms, line });
            }
            if self.peek().is_none() {
                return Err(self.unfinished());
            }
            self.eat("(");
            let mut patterns = Vec::new();
            loop {
                self.skip_blanks();
                match self.word(Ctx::Normal)? {
                    Some(pattern) => patterns.push(pattern),
                    None => {
                        return match self.peek() {
                            None => Err(self.unfinished()),
                            Some(c) => Err(self.unexpected(c)),
                        };
                    }
                }
                self.skip_blanks();
                if self.eat("|") {
                    continue;
                }
                if self.eat(")") {
                    break;
                }
                return match self.peek() {
                    None => Err(self.unfinished()),
                    Some(c) => Err(self.unexpected(c)),
                };
            }
            let body = self.list(End::Words(&["esac"]))?;
            self.linebreak()?;
            let end = if self.eat(";;&") {
                CaseEnd::Continue
            } else if self.eat(";;") {
                CaseEnd::Break
            } else if self.eat(";&") {
                CaseEnd::FallThrough
            } else if self.at_word("esac") {
                CaseEnd::Break
            } else {
                return match self.peek() {
                    None => Err(self.unfinished()),
                    Some(c) => Err(self.unexpected(c)),
                };
            };
            arms.push(CaseArm { patterns, body, end });
        }
    }

    /// `function name [()] body`.
    fn function_keyword(&mut self, line: u32) -> Result<Command> {
        self.skip_blanks();
        let start = self.pos;
        let Some(name_word) = self.word(Ctx::Normal)? else {
            self.pos = start;
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        let Some(name) = name_word.as_literal().map(str::to_string) else {
            let text: String = self.text[start..self.pos].iter().collect();
            return Err(self.syntax(format!("`{text}': not a valid identifier")));
        };
        self.skip_blanks();
        if self.at("(") {
            self.bump();
            self.skip_blanks();
            if !self.eat(")") {
                return match self.peek() {
                    None => Err(self.unfinished()),
                    Some(c) => Err(self.unexpected(c)),
                };
            }
        }
        self.function_body(name, line)
    }

    /// The compound command and redirections of a function definition.
    fn function_body(&mut self, name: String, line: u32) -> Result<Command> {
        self.linebreak()?;
        if self.peek().is_none() {
            return Err(self.unfinished());
        }
        let start = (self.pos, self.line);
        let body = self.command()?;
        let body_line = start.1;
        if !matches!(body, Command::Compound(..)) {
            // bash finds the body's first word unexpected.
            (self.pos, self.line) = start;
            let c = self.peek().unwrap_or('\n');
            return Err(self.unexpected(c));
        }
        Ok(Command::Function(Rc::new(FunctionDef {
            name,
            body,
            file: self.file.clone(),
            line,
            body_line,
        })))
    }

    fn simple_command(&mut self, line: u32) -> Result<Command> {
        let mut command = SimpleCommand { line, ..SimpleCommand::default() };
        // The command's line, as bash gives it to `LINENO` and to messages:
        // where its first assignment or redirection ends; when it starts
        // with a word, where the token after the word does, which bash
        // reads to see whether the word names a function.
        let mut placed = false;
        let mut after_word = false;
        loop {
            self.skip_blanks();
            let at = self.line;
            if let Some(redirect) = self.redirect()? {
                if !placed {
                    command.line = if after_word { at } else { self.line };
                    placed = true;
                }
                command.redirects.push(redirect);
                continue;
            }
            let process_subst = !self.options.posix && (self.at("<(") || self.at(">("));
            match self.peek() {
                None => break,
                Some(c) if is_meta(c) && !process_subst => break,
                Some(_) => {}
            }
            let declaration = command
                .words
                .first()
                .and_then(Word::as_literal)
                .is_some_and(|w| DECLARATIONS.contains(&w));
            let first = command.words.is_empty();
            if (first || declaration)
                && let Some(assign) = self.assignment(first)?
            {
                if !placed {
                    command.line = self.line;
                    placed = true;
                }
                if command.words.is_empty() {
                    command.assigns.push(assign);
                } else {
                    command
                        .words
                        .push(Word { parts: vec![WordPart::ArrayAssign(Box::new(assign))] });
                }
                continue;
            }
            let Some(word) = self.word(if first { Ctx::Command } else { Ctx::Normal })? else {
                break;
            };
            command.words.push(word);
            if !placed {
                if after_word {
                    command.line = self.line;
                    placed = true;
                }
                after_word = true;
            }
            if command.words.len() == 1
                && command.assigns.is_empty()
                && command.redirects.is_empty()
            {
                // `name ()`: a function definition.
                let name = command.words[0].as_literal().map(str::to_string);
                self.skip_blanks();
                if let Some(name) = name
                    && self.at("(")
                {
                    let start = (self.pos, self.line);
                    self.bump();
                    self.skip_blanks();
                    if self.eat(")") {
                        if RESERVED.contains(&name.as_str()) {
                            return Err(self.syntax_here(format!(
                                "syntax error near unexpected token `{name}'"
                            )));
                        }
                        return self.function_body(name, line);
                    }
                    (self.pos, self.line) = start;
                    return Err(self.unexpected('('));
                }
                // The word after an alias whose text ends with a blank.
                if self.alias_next {
                    self.alias_next = false;
                    self.expand_alias();
                }
            }
        }
        if command.words.is_empty() && command.assigns.is_empty() && command.redirects.is_empty() {
            return match self.peek() {
                None => Err(self.unfinished()),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        if !placed {
            command.line = self.line;
        }
        Ok(Command::Simple(command))
    }

    /// `name=value`, `name+=value`, `name[sub]=value` or `name=(...)`, if
    /// the next word is one; otherwise nothing is consumed. At a command's
    /// start (`command`) the subscript runs to its `]` whatever it holds,
    /// as bash reads it; elsewhere a blank or an operator ends the word.
    fn assignment(&mut self, command: bool) -> Result<Option<Assign>> {
        let (start, start_line) = (self.pos, self.line);
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if !is_name_char(c) {
                break;
            }
            name.push(c);
            self.bump();
        }
        if !is_name(&name) {
            (self.pos, self.line) = (start, start_line);
            return Ok(None);
        }
        let mut index = None;
        if !self.options.posix && self.peek() == Some('[') {
            self.bump();
            let line = self.line;
            let Some(text) = self.subscript(command) else {
                (self.pos, self.line) = (start, start_line);
                return Ok(None);
            };
            index = Some(self.sub_word(&text, line, WordMode::Subscript)?);
        }
        let append = !self.options.posix && self.eat("+=");
        if !append && !self.eat("=") {
            (self.pos, self.line) = (start, start_line);
            return Ok(None);
        }
        if !self.options.posix && index.is_none() && self.peek() == Some('(') {
            let open_line = self.line;
            self.bump();
            let mut elements = Vec::new();
            loop {
                self.linebreak()?;
                match self.peek() {
                    None => return Err(Parser::unmatched(')', open_line)),
                    Some(')') => {
                        self.bump();
                        break;
                    }
                    Some(_) => {}
                }
                let mut key = None;
                if self.peek() == Some('[') {
                    let save = (self.pos, self.line);
                    self.bump();
                    let line = self.line;
                    match self.subscript(true) {
                        Some(text) if self.eat("=") => {
                            key = Some(self.sub_word(&text, line, WordMode::Subscript)?);
                        }
                        _ => (self.pos, self.line) = save,
                    }
                }
                match self.word(Ctx::Normal)? {
                    Some(word) => elements.push((key, word)),
                    None if key.is_some() => elements.push((key, Word::default())),
                    None => match self.peek() {
                        None => return Err(Parser::unmatched(')', open_line)),
                        Some(c) => return Err(self.unexpected(c)),
                    },
                }
            }
            return Ok(Some(Assign { name, index, append, value: AssignValue::Array(elements) }));
        }
        let value = self.word(Ctx::Normal)?.unwrap_or_default();
        Ok(Some(Assign { name, index, append, value: AssignValue::Scalar(value) }))
    }

    /// The text of a subscript, after its `[`, through the `]` that
    /// closes it: `None` if none does, or, outside a command's start, if
    /// a blank or an operator comes first. A `]` quoted, escaped, or in
    /// `$(...)`, `${...}` or parentheses does not close it.
    fn subscript(&mut self, command: bool) -> Option<String> {
        let mut text = String::new();
        let mut depth = 0;
        let mut nested = Vec::new();
        loop {
            let c = self.peek()?;
            match c {
                ']' if depth == 0 && nested.is_empty() => {
                    self.bump();
                    return Some(text);
                }
                // A quoted subscript, as in `m["a key"]=1`: up to the
                // closing quote whatever it holds.
                '\'' | '"' => {
                    text.push(c);
                    self.bump();
                    loop {
                        let inner = self.peek()?;
                        text.push(inner);
                        self.bump();
                        if inner == '\\' && c == '"' {
                            if let Some(escaped) = self.peek() {
                                text.push(escaped);
                                self.bump();
                            }
                        } else if inner == c {
                            break;
                        }
                    }
                }
                '\\' => {
                    text.push(c);
                    self.bump();
                    if let Some(escaped) = self.peek() {
                        text.push(escaped);
                        self.bump();
                    }
                }
                c if is_meta(c) && !command => return None,
                _ => {
                    match c {
                        '[' if nested.is_empty() => depth += 1,
                        ']' if nested.is_empty() => depth -= 1,
                        '(' => nested.push(')'),
                        '{' if text.ends_with('$') => nested.push('}'),
                        ')' | '}' if nested.last() == Some(&c) => {
                            nested.pop();
                        }
                        _ => {}
                    }
                    text.push(c);
                    self.bump();
                }
            }
        }
    }

    /// Redirections after a compound command.
    fn redirects(&mut self) -> Result<Vec<Redirect>> {
        let mut redirects = Vec::new();
        loop {
            self.skip_blanks();
            match self.redirect()? {
                Some(redirect) => redirects.push(redirect),
                None => return Ok(redirects),
            }
        }
    }

    /// A redirection, if one is next.
    fn redirect(&mut self) -> Result<Option<Redirect>> {
        let (start, start_line) = (self.pos, self.line);
        let mut fd = None;
        // A descriptor's number, or `{name}`, right before the operator.
        let mut digits = String::new();
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            digits.push(c);
            self.bump();
        }
        if !digits.is_empty() {
            if matches!(self.peek(), Some('<' | '>')) {
                fd = digits.parse().ok().map(RedirFd::Number);
                if fd.is_none() {
                    return Err(self.syntax(format!("{digits} is not a file descriptor")));
                }
            } else {
                (self.pos, self.line) = (start, start_line);
                return Ok(None);
            }
        } else if !self.options.posix && self.peek() == Some('{') {
            self.bump();
            let mut name = String::new();
            while let Some(c) = self.peek() {
                if !is_name_char(c) {
                    break;
                }
                name.push(c);
                self.bump();
            }
            // `{name[subscript]}`, an element, as bash takes it.
            if is_name(&name) && self.peek() == Some('[') {
                let mut depth = 0;
                while let Some(c) = self.peek() {
                    if matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>') {
                        break;
                    }
                    name.push(c);
                    self.bump();
                    match c {
                        '[' => depth += 1,
                        ']' => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        break;
                    }
                }
                if depth != 0 || name.ends_with("[]") {
                    name.clear();
                }
            }
            if is_name(name.split('[').next().unwrap_or(""))
                && self.eat("}")
                && matches!(self.peek(), Some('<' | '>'))
            {
                fd = Some(RedirFd::Var(name));
            } else {
                (self.pos, self.line) = (start, start_line);
                return Ok(None);
            }
        }
        // `<(...)` and `>(...)` are words: process substitutions.
        if !self.options.posix && fd.is_none() && (self.at("<(") || self.at(">(")) {
            (self.pos, self.line) = (start, start_line);
            return Ok(None);
        }
        let op = if !self.options.posix && fd.is_none() && self.eat("&>>") {
            RedirOp::AppendBoth
        } else if !self.options.posix && fd.is_none() && self.eat("&>") {
            RedirOp::WriteBoth
        } else if !self.options.posix && self.eat("<<<") {
            RedirOp::HereString
        } else if self.at("<<-") {
            self.eat("<<-");
            return self.heredoc(fd, true).map(Some);
        } else if self.eat("<<") {
            return self.heredoc(fd, false).map(Some);
        } else if self.eat("<>") {
            RedirOp::ReadWrite
        } else if self.eat("<&") {
            RedirOp::DupRead
        } else if self.eat("<") {
            if self.peek() == Some('(') && !self.options.posix {
                // `<(...)` is a word, not a redirection.
                (self.pos, self.line) = (start, start_line);
                return Ok(None);
            }
            RedirOp::Read
        } else if self.eat(">>") {
            RedirOp::Append
        } else if self.eat(">|") {
            RedirOp::Clobber
        } else if self.eat(">&") {
            RedirOp::DupWrite
        } else if self.eat(">") {
            if self.peek() == Some('(') && !self.options.posix {
                (self.pos, self.line) = (start, start_line);
                return Ok(None);
            }
            RedirOp::Write
        } else {
            (self.pos, self.line) = (start, start_line);
            return Ok(None);
        };
        self.skip_blanks();
        // After `>&` or `<&`, a `-` is a word of its own, as in bash's
        // lexer: `>&-1` closes the output and gives the command a 1.
        if matches!(op, RedirOp::DupRead | RedirOp::DupWrite) && self.peek() == Some('-') {
            self.bump();
            return Ok(Some(Redirect { fd, op, target: RedirTarget::Word(Word::lit("-")) }));
        }
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.ends_here()),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        Ok(Some(Redirect { fd, op, target: RedirTarget::Word(word) }))
    }

    /// `<<` or `<<-` and its delimiter; the body is read after the line.
    fn heredoc(&mut self, fd: Option<RedirFd>, strip_tabs: bool) -> Result<Redirect> {
        self.skip_blanks();
        let start = self.pos;
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.ends_here()),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        let expand = !word.has_quotes();
        let delimiter = word.unquoted_text();
        let written = self.text[start..self.pos].iter().filter(|&&c| c != '\n').collect();
        let doc = Rc::new(RefCell::new(HereDoc {
            body: Word::default(),
            expand,
            delimiter: delimiter.clone(),
            written,
            strip_tabs,
        }));
        let line = self.line;
        self.pending.push(Pending { doc: doc.clone(), delimiter, strip_tabs, line });
        Ok(Redirect { fd, op: RedirOp::HereDoc, target: RedirTarget::HereDoc(doc) })
    }

    /// Reads the bodies of the pending here-documents, which start here,
    /// after a newline.
    fn read_heredocs(&mut self) -> Result<()> {
        let pending = std::mem::take(&mut self.pending);
        for doc in pending {
            let first_line = self.line;
            let mut body = String::new();
            loop {
                if self.peek_raw().is_none() {
                    // As bash and dash do, the real end of the text ends it
                    // too, but a prompt or a script reads on.
                    if self.more {
                        return Err(Error::Incomplete(Unfinished::HereDoc(doc.delimiter)));
                    }
                    self.ended_heredoc(&doc);
                    break;
                }
                let mut text = String::new();
                while let Some(c) = self.bump_raw() {
                    if c == '\n' {
                        break;
                    }
                    text.push(c);
                }
                let line = if doc.strip_tabs { text.trim_start_matches('\t') } else { &text };
                if line == doc.delimiter {
                    break;
                }
                body.push_str(line);
                body.push('\n');
            }
            let expand = doc.doc.borrow().expand;
            let word = if expand {
                self.sub_word(&body, first_line, WordMode::HereDoc)?
            } else {
                Word { parts: vec![WordPart::Quoted(body)] }
            };
            doc.doc.borrow_mut().body = word;
        }
        Ok(())
    }

    /// At the end of the text, after a command: here-documents still
    /// waiting for their bodies wait for more text, or at its real end,
    /// are empty.
    fn finish_heredocs(&mut self) -> Result<()> {
        if let Some(doc) = self.pending.first() {
            if self.more {
                return Err(Error::Incomplete(Unfinished::HereDoc(doc.delimiter.clone())));
            }
            for doc in std::mem::take(&mut self.pending) {
                self.ended_heredoc(&doc);
            }
        }
        Ok(())
    }

    /// bash's warning for a here-document the end of the text ended, at
    /// the last line read; dash, which POSIX mode follows, is silent.
    fn ended_heredoc(&mut self, doc: &Pending) {
        if self.options.posix {
            return;
        }
        let at =
            if self.text.last() == Some(&'\n') { self.line.saturating_sub(1) } else { self.line };
        self.warnings.push((
            at.max(1),
            format!(
                "warning: here-document at line {} delimited by end-of-file (wanted `{}')",
                doc.line, doc.delimiter
            ),
        ));
    }

    // `[[ ]]`, read as bash reads it: newlines may come where a term
    // starts and after a whole one, not inside one, and its errors are
    // bash's.

    /// `[[ expression ]]`, after the `[[`.
    fn cond_command(&mut self) -> Result<CondExpr> {
        let line = std::mem::replace(&mut self.cond_line, self.line);
        let parens = std::mem::take(&mut self.cond_parens);
        let result = self.cond_command_here();
        (self.cond_line, self.cond_parens) = (line, parens);
        result
    }

    fn cond_command_here(&mut self) -> Result<CondExpr> {
        let expr = self.cond_or()?;
        let next = self.cond_next();
        match next {
            CondNext::Close => {
                self.eat("]]");
                Ok(expr)
            }
            CondNext::End => Err(Error::Incomplete(Unfinished::Cond {
                line: self.cond_line,
                parens: Vec::new(),
                term: false,
            })),
            _ => {
                let message = match next.name() {
                    Some(token) => {
                        format!(
                            "syntax error in conditional expression: unexpected token `{token}'"
                        )
                    }
                    None => "syntax error in conditional expression".into(),
                };
                let at = self.cond_skip(next)?;
                Err(self.near_error(vec![(self.cond_line, message)], at))
            }
        }
    }

    /// Blanks, comments and newlines, where `[[ ]]` takes them.
    fn skip_cond_blanks(&mut self) -> Result<()> {
        loop {
            self.skip_blanks();
            if self.peek() == Some('\n') {
                self.newline()?;
            } else {
                return Ok(());
            }
        }
    }

    fn cond_or(&mut self) -> Result<CondExpr> {
        let mut left = self.cond_and()?;
        while self.eat("||") {
            let right = self.cond_and()?;
            left = CondExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn cond_and(&mut self) -> Result<CondExpr> {
        let mut left = self.cond_term()?;
        while self.eat("&&") {
            let right = self.cond_term()?;
            left = CondExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// A term, and the blanks and newlines after it but for a lone word's.
    fn cond_term(&mut self) -> Result<CondExpr> {
        self.nested(Parser::cond_term_here)
    }

    fn cond_term_here(&mut self) -> Result<CondExpr> {
        self.skip_cond_blanks()?;
        let line = self.line;
        let next = self.cond_next();
        match next {
            CondNext::End => Err(Error::Incomplete(Unfinished::Cond {
                line: self.cond_line,
                parens: self.cond_parens.clone(),
                term: true,
            })),
            CondNext::Close => {
                let at = self.cond_skip(next)?;
                Err(self.cond_error(None, at))
            }
            CondNext::Op("(") => {
                self.eat("(");
                self.cond_parens.push(line);
                let inner = self.cond_or()?;
                if !self.eat(")") {
                    return Err(self.cond_unclosed());
                }
                self.cond_parens.pop();
                self.skip_cond_blanks()?;
                Ok(CondExpr::Group(Box::new(inner)))
            }
            CondNext::Word if self.at_word("!") => {
                self.eat("!");
                Ok(CondExpr::Not(Box::new(self.cond_term()?)))
            }
            CondNext::Word => self.cond_test(),
            CondNext::Op(_) | CondNext::Newline => {
                let token = next.name().unwrap_or_default();
                let at = self.cond_skip(next)?;
                Err(self.cond_error(
                    Some(format!("unexpected token `{token}' in conditional command")),
                    at,
                ))
            }
        }
    }

    /// A term that starts with a word: `-f file`, `a == b`, or the word
    /// alone, which `]]`, `&&`, `||` or `)` must follow.
    fn cond_test(&mut self) -> Result<CondExpr> {
        let Some(first) = self.word(Ctx::Cond)? else {
            return Err(self.cond_error(None, self.pos));
        };
        if let Some(op) = first.as_literal()
            && is_unary_test(op)
        {
            let op = op.to_string();
            let next = self.cond_next_in_term()?;
            if next == CondNext::Word
                && let Some(operand) = self.word(Ctx::Cond)?
            {
                self.skip_cond_blanks()?;
                return Ok(CondExpr::Unary(op, operand));
            }
            let token = next.name().unwrap_or_default();
            let at = self.cond_skip(next)?;
            let message = format!("unexpected argument `{token}' to conditional unary operator");
            return Err(self.cond_error(Some(message), at));
        }
        let next = self.cond_next_in_term()?;
        let op = match next {
            CondNext::Word => match BINARY_TESTS.iter().find(|op| self.at_word(op)) {
                Some(op) => op,
                None => {
                    let at = self.cond_skip(next)?;
                    let message = "conditional binary operator expected".to_string();
                    return Err(self.cond_error(Some(message), at));
                }
            },
            CondNext::Op(op @ ("<" | ">")) => op,
            CondNext::Close | CondNext::Op("&&" | "||" | ")") => return Ok(CondExpr::Word(first)),
            _ => {
                let token = next.name().unwrap_or_default();
                let at = self.cond_skip(next)?;
                let message =
                    format!("unexpected token `{token}', conditional binary operator expected");
                return Err(self.cond_error(Some(message), at));
            }
        };
        self.eat(op);
        let next = self.cond_next_in_term()?;
        let right = match next {
            // A regular expression is read up to a blank or a
            // metacharacter outside its groups: it may be empty.
            CondNext::Word | CondNext::Op(_) if op == "=~" => {
                self.word(Ctx::Regex)?.unwrap_or_default()
            }
            CondNext::Word if matches!(op, "==" | "!=" | "=") => {
                // A pattern, read as with extglob on, as bash reads it.
                let extglob = std::mem::replace(&mut self.options.extglob, true);
                let word = self.word(Ctx::Cond);
                self.options.extglob = extglob;
                word?.unwrap_or_default()
            }
            CondNext::Word => self.word(Ctx::Cond)?.unwrap_or_default(),
            _ => {
                let token = next.name().unwrap_or_default();
                let at = self.cond_skip(next)?;
                let message =
                    format!("unexpected argument `{token}' to conditional binary operator");
                return Err(self.cond_error(Some(message), at));
            }
        };
        self.skip_cond_blanks()?;
        Ok(CondExpr::Binary(first, op.to_string(), right))
    }

    /// What comes next in `[[ ]]`.
    fn cond_next(&mut self) -> CondNext {
        match self.peek() {
            None => CondNext::End,
            Some('\n') => CondNext::Newline,
            // A process substitution is a word.
            Some('<' | '>') if self.peek2() == Some('(') => CondNext::Word,
            Some(c) if is_meta(c) => match OPERATORS.iter().find(|op| self.at(op)) {
                Some(op) => CondNext::Op(op),
                None => CondNext::Word,
            },
            Some(_) if self.at_word("]]") => CondNext::Close,
            Some(_) => CondNext::Word,
        }
    }

    /// What comes next inside a term, after blanks: where bash, which ends
    /// text with a newline, finds a newline at the real end.
    fn cond_next_in_term(&mut self) -> Result<CondNext> {
        self.skip_blanks();
        match self.cond_next() {
            CondNext::End if self.more => Err(self.unfinished()),
            CondNext::End => Ok(CondNext::Newline),
            next => Ok(next),
        }
    }

    /// Reads `next`, which an error is about, and gives where it ends.
    fn cond_skip(&mut self, next: CondNext) -> Result<usize> {
        match next {
            CondNext::Word => {
                self.word(Ctx::Cond)?;
            }
            CondNext::Op(op) => {
                self.eat(op);
            }
            CondNext::Close => {
                self.eat("]]");
            }
            CondNext::End | CondNext::Newline => {}
        }
        Ok(self.pos)
    }

    /// bash's error for `[[ ]]` text it cannot read: `message`, at the line
    /// being read, the "expected `)'" of each parenthesis open around it,
    /// innermost first, and what it is near.
    fn cond_error(&self, message: Option<String>, at: usize) -> Error {
        let mut messages: Vec<_> =
            message.map(|message| (self.line, message)).into_iter().collect();
        messages.extend(self.cond_parens.iter().rev().map(|&line| (line, "expected `)'".into())));
        self.near_error(messages, at)
    }

    /// bash's error for a `(` in `[[ ]]` whose expression is followed by
    /// something other than its `)`.
    fn cond_unclosed(&mut self) -> Error {
        let next = self.cond_next();
        if next == CondNext::End {
            return Error::Incomplete(Unfinished::Cond {
                line: self.cond_line,
                parens: self.cond_parens.clone(),
                term: false,
            });
        }
        let first = match next.name() {
            Some(token) => format!("unexpected token `{token}', expected `)'"),
            None => "expected `)'".into(),
        };
        let at = match self.cond_skip(next) {
            Ok(at) => at,
            Err(error) => return error,
        };
        let mut messages = Vec::new();
        if let Some((&inner, outer)) = self.cond_parens.split_last() {
            messages.push((inner, first));
            messages.extend(outer.iter().rev().map(|&line| (line, "expected `)'".into())));
        }
        self.near_error(messages, at)
    }

    /// `messages`, then bash's "syntax error near" what its reader stopped
    /// before at `at`, with the line it is on.
    fn near_error(&self, mut messages: Vec<(u32, String)>, at: usize) -> Error {
        if let Some(text) = self.near(at) {
            messages.push((self.line, format!("syntax error near `{text}'")));
        }
        Error::Syntax { messages, context: Some(self.line_at(at)) }
    }

    /// What bash names in "syntax error near `...'" when its reader stopped
    /// before `at`: back from there, past blanks, to a blank, `;`, `|` or
    /// `&` on that line, with the character at `at` when it is not blank
    /// (so `&` for `&&`, and `(b` for a `(` before `b`).
    fn near(&self, at: usize) -> Option<String> {
        let at = at.min(self.text.len());
        let start = self.text[..at].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let line: Vec<char> =
            self.text[start..].iter().copied().take_while(|&c| c != '\n').chain(['\n']).collect();
        let blank = |c: char| matches!(c, ' ' | '\t' | '\n');
        let mut i = (at - start).min(line.len() - 1);
        while i > 0 && blank(line[i]) {
            i -= 1;
        }
        let end = i + 1;
        while i > 0 && !matches!(line[i], ' ' | '\t' | '\n' | ';' | '|' | '&') {
            i -= 1;
        }
        while i < end && blank(line[i]) {
            i += 1;
        }
        (i < end).then(|| line[i..end].iter().collect())
    }

    // Words.

    /// Reads a word; `None` if one does not start here.
    fn word(&mut self, ctx: Ctx) -> Result<Option<Word>> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut paren_depth = 0usize;
        let mut paren_line = self.line;
        let mut bracket_depth = 0usize;
        let mut bracket_line = self.line;
        while let Some(c) = self.peek() {
            match c {
                '[' if ctx == Ctx::Command
                    && (bracket_depth > 0 || parts.is_empty() && is_name(&lit)) =>
                {
                    if bracket_depth == 0 {
                        bracket_line = self.line;
                    }
                    bracket_depth += 1;
                    lit.push(c);
                    self.bump();
                }
                ']' if bracket_depth > 0 => {
                    bracket_depth -= 1;
                    lit.push(c);
                    self.bump();
                }
                c if bracket_depth > 0 && is_meta(c) => {
                    lit.push(c);
                    self.bump();
                }
                '(' if ctx == Ctx::Regex => {
                    if paren_depth == 0 {
                        paren_line = self.line;
                    }
                    paren_depth += 1;
                    lit.push(c);
                    self.bump();
                }
                ')' if ctx == Ctx::Regex && paren_depth > 0 => {
                    paren_depth -= 1;
                    lit.push(c);
                    self.bump();
                }
                // Outside parentheses too, as bash has it: `a|b`.
                '|' if ctx == Ctx::Regex => {
                    lit.push(c);
                    self.bump();
                }
                ' ' | '\t' | '\n' if ctx == Ctx::Regex && paren_depth > 0 => {
                    lit.push(c);
                    self.bump();
                }
                '<' | '>' if lit.is_empty() && parts.is_empty() && !self.options.posix => {
                    // `<(...)` and `>(...)`.
                    if self.peek2() == Some('(') && ctx != Ctx::Regex {
                        let output = c == '>';
                        self.bump();
                        self.bump();
                        let program = self.list(End::Paren)?;
                        self.linebreak()?;
                        if !self.eat(")") {
                            return match self.peek() {
                                None => Err(Error::Incomplete(Unfinished::Matching {
                                    close: ')',
                                    line: None,
                                })),
                                Some(c) => Err(self.unexpected(c)),
                            };
                        }
                        parts.push(WordPart::ProcSub(output, Rc::new(program)));
                        continue;
                    }
                    break;
                }
                c if is_meta(c) => break,
                '#' if lit.is_empty() && parts.is_empty() => {
                    // A comment, not a word.
                    break;
                }
                '\\' => {
                    self.bump();
                    match self.bump_raw() {
                        Some(escaped) => {
                            flush(&mut lit, &mut parts);
                            parts.push(WordPart::Escaped(escaped));
                        }
                        // A backslash at the very end stands for itself.
                        None => lit.push('\\'),
                    }
                }
                '\'' => {
                    let quote_line = self.line;
                    self.bump();
                    flush(&mut lit, &mut parts);
                    let mut text = String::new();
                    loop {
                        match self.bump_raw() {
                            Some('\'') => break,
                            Some(c) => text.push(c),
                            None => return Err(Parser::unmatched('\'', quote_line)),
                        }
                    }
                    parts.push(WordPart::Quoted(text));
                }
                '"' => {
                    self.bump();
                    flush(&mut lit, &mut parts);
                    let inner = self.double_quoted()?;
                    parts.push(WordPart::Double(inner));
                }
                '`' => {
                    self.bump();
                    flush(&mut lit, &mut parts);
                    parts.push(self.backquoted(false)?);
                }
                '$' => {
                    flush(&mut lit, &mut parts);
                    match self.dollar(false)? {
                        Some(part) => parts.push(part),
                        None => lit.push('$'),
                    }
                }
                '@' | '!' | '+' | '*' | '?'
                    if self.options.extglob && self.peek2() == Some('(') =>
                {
                    // An extended pattern: its parentheses are part of the
                    // word.
                    let open_line = self.line;
                    lit.push(c);
                    self.bump();
                    self.bump();
                    lit.push('(');
                    let mut depth = 1;
                    while depth > 0 {
                        match self.bump() {
                            None => return Err(Parser::unmatched(')', open_line)),
                            Some(c) => {
                                if c == '(' {
                                    depth += 1;
                                } else if c == ')' {
                                    depth -= 1;
                                }
                                lit.push(c);
                            }
                        }
                    }
                }
                _ => {
                    lit.push(c);
                    self.bump();
                }
            }
        }
        if paren_depth > 0 {
            return Err(Error::Incomplete(Unfinished::Regex(paren_line)));
        }
        if bracket_depth > 0 {
            return Err(Parser::unmatched(']', bracket_line));
        }
        flush(&mut lit, &mut parts);
        if parts.is_empty() { Ok(None) } else { Ok(Some(Word { parts })) }
    }

    /// The inside of `"..."`, after the opening quote, through the
    /// closing one.
    fn double_quoted(&mut self) -> Result<Vec<WordPart>> {
        let line = self.line;
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            let Some(c) = self.peek() else { return Err(Parser::unmatched('"', line)) };
            match c {
                '"' => {
                    self.bump();
                    break;
                }
                '\\' => {
                    self.bump();
                    match self.bump_raw() {
                        Some(c @ ('$' | '`' | '"' | '\\')) => text.push(c),
                        Some(c) => {
                            text.push('\\');
                            text.push(c);
                        }
                        None => return Err(Parser::unmatched('"', line)),
                    }
                }
                '`' => {
                    self.bump();
                    flush_quoted(&mut text, &mut parts);
                    parts.push(self.backquoted(true)?);
                }
                '$' => {
                    flush_quoted(&mut text, &mut parts);
                    match self.dollar(true)? {
                        Some(part) => parts.push(part),
                        None => text.push('$'),
                    }
                }
                _ => {
                    text.push(c);
                    self.bump();
                }
            }
        }
        flush_quoted(&mut text, &mut parts);
        Ok(parts)
    }

    /// `` `...` ``, after the opening quote: a command substitution, whose
    /// text loses the backslashes before `$`, `` ` `` and `\` (and `"`
    /// inside double quotes) and is then parsed.
    fn backquoted(&mut self, in_double: bool) -> Result<WordPart> {
        let line = self.line;
        let mut text = String::new();
        loop {
            match self.bump_raw() {
                None => return Err(Parser::unmatched('`', line)),
                Some('`') => break,
                Some('\\') => match self.bump_raw() {
                    Some(c @ ('$' | '`' | '\\')) => text.push(c),
                    Some('"') if in_double => text.push('"'),
                    Some('\n') => {}
                    Some(c) => {
                        text.push('\\');
                        text.push(c);
                    }
                    None => return Err(Parser::unmatched('`', line)),
                },
                Some(c) => text.push(c),
            }
        }
        let program = self.sub_program(&text, line)?;
        Ok(WordPart::Command(Rc::new(program)))
    }

    /// What follows a `$`: `None` (with only the `$` consumed) if nothing
    /// that expands does.
    fn dollar(&mut self, in_double: bool) -> Result<Option<WordPart>> {
        self.nested(|parser| parser.dollar_here(in_double))
    }

    fn dollar_here(&mut self, in_double: bool) -> Result<Option<WordPart>> {
        self.bump();
        let Some(c) = self.peek() else { return Ok(None) };
        match c {
            '(' if self.at("((") => {
                let (pos, line) = (self.pos, self.line);
                let start_line = self.line;
                self.eat("((");
                if let Some(text) = self.arith_text()? {
                    let word = self.sub_word(&text, start_line, WordMode::Arith)?;
                    return Ok(Some(WordPart::Arith(word)));
                }
                // `$( (...) )`: a command substitution of a subshell.
                (self.pos, self.line) = (pos, line);
                self.command_substitution()
            }
            '(' => self.command_substitution(),
            '{' => {
                self.bump();
                let param = self.brace_param(in_double)?;
                Ok(Some(WordPart::Param(Box::new(param))))
            }
            '\'' if !in_double => {
                self.bump();
                let text = self.ansi_c()?;
                Ok(Some(WordPart::Quoted(text)))
            }
            '"' if !in_double => {
                // `$"..."`, a string to translate: here, as it is.
                self.bump();
                let inner = self.double_quoted()?;
                Ok(Some(WordPart::Double(inner)))
            }
            c if is_name_start(c) => {
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if !is_name_char(c) {
                        break;
                    }
                    name.push(c);
                    self.bump();
                }
                Ok(Some(param(ParamName::Var(name))))
            }
            c if c.is_ascii_digit() => {
                self.bump();
                let name = if c == '0' {
                    ParamName::Special('0')
                } else {
                    ParamName::Positional(c as usize - '0' as usize)
                };
                Ok(Some(param(name)))
            }
            '@' | '*' | '#' | '?' | '-' | '$' | '!' => {
                self.bump();
                Ok(Some(param(ParamName::Special(c))))
            }
            _ => Ok(None),
        }
    }

    fn command_substitution(&mut self) -> Result<Option<WordPart>> {
        self.bump();
        let program = self.list(End::Paren)?;
        self.linebreak()?;
        if !self.eat(")") {
            return match self.peek() {
                None => Err(Error::Incomplete(Unfinished::Matching { close: ')', line: None })),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        Ok(Some(WordPart::Command(Rc::new(program))))
    }

    /// `$'...'`, after the quote: its escapes decoded.
    fn ansi_c(&mut self) -> Result<String> {
        let line = self.line;
        let mut out = String::new();
        loop {
            let Some(c) = self.bump_raw() else { return Err(Parser::unmatched('\'', line)) };
            match c {
                '\'' => return Ok(out),
                '\\' => {
                    let Some(e) = self.bump_raw() else {
                        return Err(Parser::unmatched('\'', line));
                    };
                    match e {
                        'a' => out.push('\x07'),
                        'b' => out.push('\x08'),
                        'e' | 'E' => out.push('\x1b'),
                        'f' => out.push('\x0c'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'v' => out.push('\x0b'),
                        '\\' | '\'' | '"' | '?' => out.push(e),
                        'c' => {
                            if let Some(ctl) = self.bump_raw() {
                                out.push(char::from((ctl.to_ascii_uppercase() as u8) ^ 0x40));
                            }
                        }
                        '0'..='7' => {
                            let mut value = e.to_digit(8).unwrap_or(0);
                            for _ in 0..2 {
                                match self.peek_raw().and_then(|c| c.to_digit(8)) {
                                    Some(d) => {
                                        value = value * 8 + d;
                                        self.pos += 1;
                                    }
                                    None => break,
                                }
                            }
                            push_code(&mut out, value & 0xff, true);
                        }
                        'x' | 'u' | 'U' => {
                            let max = match e {
                                'x' => 2,
                                'u' => 4,
                                _ => 8,
                            };
                            let mut value = 0u32;
                            let mut count = 0;
                            while count < max {
                                match self.peek_raw().and_then(|c| c.to_digit(16)) {
                                    Some(d) => {
                                        value = value.wrapping_mul(16) + d;
                                        self.pos += 1;
                                        count += 1;
                                    }
                                    None => break,
                                }
                            }
                            if count == 0 {
                                out.push('\\');
                                out.push(e);
                            } else {
                                push_code(&mut out, value, e == 'x');
                            }
                        }
                        _ => {
                            out.push('\\');
                            out.push(e);
                        }
                    }
                }
                _ => out.push(c),
            }
        }
    }

    /// `${...}`, after the `{`. One the shell cannot take is read to its
    /// `}`, an error only if it is expanded, as in bash and dash: a script
    /// may hold, where it never runs it, an expansion of another shell
    /// (`if [ -n "$BASH_VERSION" ]; then x=${y/a/b}; fi` in POSIX mode).
    fn brace_param(&mut self, in_double: bool) -> Result<ParamExp> {
        let (pos, line, depth, pending) = (self.pos, self.line, self.depth, self.pending.len());
        match self.brace_param_parts(in_double) {
            Err(Error::Syntax { messages, .. })
                if messages.last().is_some_and(|(_, text)| text.ends_with("bad substitution")) =>
            {
                self.pos = pos;
                self.line = line;
                self.depth = depth;
                self.pending.truncate(pending);
                let text = self.brace_text(in_double)?;
                let name = ParamName::Special('?');
                Ok(ParamExp {
                    name,
                    index: None,
                    op: ParamOp::Bad(text),
                    indirect: false,
                    braced: true,
                })
            }
            other => other,
        }
    }

    /// The text of a `${...}` after its `{`, through the `}` that closes
    /// it: a `}` in quotes, after a backslash, or closing a `{` within
    /// does not. Inside double quotes, a single quote is itself.
    fn brace_text(&mut self, in_double: bool) -> Result<String> {
        let mut text = String::new();
        let (mut braces, mut parens) = (0usize, 0usize);
        let mut quote = None;
        loop {
            let Some(c) = self.bump() else { return Err(self.unmatched_here('}')) };
            if let Some(open) = quote {
                text.push(c);
                if c == open {
                    quote = None;
                } else if c == '\\'
                    && open != '\''
                    && let Some(next) = self.bump()
                {
                    text.push(next);
                }
                continue;
            }
            match c {
                '}' if braces == 0 && parens == 0 => return Ok(text),
                '\\' => {
                    text.push(c);
                    if let Some(next) = self.bump() {
                        text.push(next);
                    }
                    continue;
                }
                '\'' if in_double => {}
                '\'' | '"' | '`' => quote = Some(c),
                '{' => braces += 1,
                '}' if braces > 0 => braces -= 1,
                '(' => parens += 1,
                ')' if parens > 0 => parens -= 1,
                _ => {}
            }
            text.push(c);
        }
    }

    /// The parts of a `${...}`, after the `{`.
    fn brace_param_parts(&mut self, in_double: bool) -> Result<ParamExp> {
        let unclosed = |p: &Parser| p.unmatched_here('}');
        let mut length = false;
        let mut indirect = false;
        // `${#}` is the count of arguments and `${#name}` a length; `${#`
        // and an operator is `$#` with it (`${#-x}`, `${##x}`, `${#@Q}`),
        // but `${#-}`, `${#?}`, `${##}` and `${#@}` are lengths, and an
        // operator with nothing to work on an error, as bash reads them.
        if self.peek() == Some('#') {
            let third = self.text.get(self.pos + 2).copied();
            match self.peek2() {
                None | Some('}') => {}
                Some('-' | '?' | '#' | '@') if third == Some('}') => {
                    self.bump();
                    length = true;
                }
                Some('%' | ':' | '=' | '+' | '/') if third == Some('}') => {
                    return Err(self.bad_substitution());
                }
                Some('#' | '%' | ':' | '-' | '=' | '?' | '+' | '/' | '@') => {}
                Some(_) => {
                    self.bump();
                    length = true;
                }
            }
        }
        // In POSIX mode, `!` is only `$!`: `${!#pattern}` takes a prefix
        // off it, as in dash.
        if !length
            && !self.options.posix
            && self.peek() == Some('!')
            && !matches!(self.peek2(), Some('}'))
        {
            self.bump();
            indirect = true;
        }
        let name = match self.peek() {
            None => return Err(unclosed(self)),
            Some(c) if is_name_start(c) => {
                let mut name = String::new();
                while let Some(c) = self.peek() {
                    if !is_name_char(c) {
                        break;
                    }
                    name.push(c);
                    self.bump();
                }
                ParamName::Var(name)
            }
            Some(c) if c.is_ascii_digit() => {
                let mut digits = String::new();
                while let Some(c) = self.peek() {
                    if !c.is_ascii_digit() {
                        break;
                    }
                    digits.push(c);
                    self.bump();
                }
                if digits == "0" {
                    ParamName::Special('0')
                } else {
                    match digits.parse() {
                        Ok(n) => ParamName::Positional(n),
                        Err(_) => {
                            return Err(self.syntax(format!("${{{digits}}}: bad substitution")));
                        }
                    }
                }
            }
            Some(c) if "@*#?-$!".contains(c) => {
                self.bump();
                ParamName::Special(c)
            }
            Some(_) => return Err(self.bad_substitution()),
        };
        let mut index = None;
        if matches!(name, ParamName::Var(_)) && self.peek() == Some('[') {
            self.bump();
            let line = self.line;
            let Some(text) = self.subscript(true) else { return Err(unclosed(self)) };
            index = Some(if text == "@" || text == "*" {
                Word::lit(&text)
            } else {
                self.sub_word(&text, line, WordMode::Subscript)?
            });
        }
        if length {
            if !self.eat("}") {
                return Err(self.bad_substitution());
            }
            return Ok(ParamExp {
                name,
                index,
                op: ParamOp::Length,
                indirect: false,
                braced: true,
            });
        }
        if indirect {
            // `${!prefix*}`, `${!prefix@}` and `${!name[@]}`.
            if index.is_none() && (self.at("*}") || self.at("@}")) {
                let all = self.at("@}");
                self.bump();
                self.bump();
                return Ok(ParamExp {
                    name,
                    index,
                    op: ParamOp::Names(all),
                    indirect: false,
                    braced: true,
                });
            }
            if let Some(sub) = index.as_ref().and_then(Word::as_literal)
                && (sub == "@" || sub == "*")
                && self.peek() == Some('}')
            {
                let all = sub == "@";
                self.bump();
                return Ok(ParamExp {
                    name,
                    index: None,
                    op: ParamOp::Keys(all),
                    indirect: false,
                    braced: true,
                });
            }
        }
        let op = self.param_op(in_double)?;
        Ok(ParamExp { name, index, op, indirect, braced: true })
    }

    fn bad_substitution(&mut self) -> Error {
        // Skip to the closing brace, so that the message can name it all.
        self.syntax("bad substitution")
    }

    /// The operator and operands of `${name...}`, through the `}`.
    fn param_op(&mut self, in_double: bool) -> Result<ParamOp> {
        let Some(c) = self.bump() else { return Err(self.unmatched_here('}')) };
        let op = match c {
            '}' => return Ok(ParamOp::None),
            ':' => match self.peek() {
                Some('-') => {
                    self.bump();
                    ParamOp::Default(true, self.brace_word(in_double, false)?)
                }
                Some('=') => {
                    self.bump();
                    ParamOp::Assign(true, self.brace_word(in_double, false)?)
                }
                Some('?') => {
                    self.bump();
                    ParamOp::Error(true, self.brace_word(in_double, false)?)
                }
                Some('+') => {
                    self.bump();
                    ParamOp::Alternative(true, self.brace_word(in_double, false)?)
                }
                _ => {
                    let line = self.line;
                    let (offset, more) = self.substring_part()?;
                    let offset = self.sub_word(&offset, line, WordMode::Arith)?;
                    let length = if more {
                        let (length, _) = self.substring_part()?;
                        Some(self.sub_word(&length, line, WordMode::Arith)?)
                    } else {
                        None
                    };
                    return Ok(ParamOp::Substring(offset, length));
                }
            },
            '-' => ParamOp::Default(false, self.brace_word(in_double, false)?),
            '=' => ParamOp::Assign(false, self.brace_word(in_double, false)?),
            '?' => ParamOp::Error(false, self.brace_word(in_double, false)?),
            '+' => ParamOp::Alternative(false, self.brace_word(in_double, false)?),
            '#' => {
                let longest = self.peek() == Some('#');
                if longest {
                    self.bump();
                }
                ParamOp::RemovePrefix(longest, self.brace_word(in_double, true)?)
            }
            '%' => {
                let longest = self.peek() == Some('%');
                if longest {
                    self.bump();
                }
                ParamOp::RemoveSuffix(longest, self.brace_word(in_double, true)?)
            }
            '/' if !self.options.posix => {
                let kind = match self.peek() {
                    Some('/') => {
                        self.bump();
                        ReplaceKind::All
                    }
                    Some('#') => {
                        self.bump();
                        ReplaceKind::Prefix
                    }
                    Some('%') => {
                        self.bump();
                        ReplaceKind::Suffix
                    }
                    _ => ReplaceKind::First,
                };
                let (pattern, more) = self.replace_pattern(in_double)?;
                let replacement =
                    if more { Some(self.brace_word(in_double, false)?) } else { None };
                return Ok(ParamOp::Replace(kind, pattern, replacement));
            }
            '^' | ',' if !self.options.posix => {
                let upper = c == '^';
                let all = self.peek() == Some(c);
                if all {
                    self.bump();
                }
                let pattern = self.brace_word(in_double, true)?;
                let pattern = if pattern.parts.is_empty() { None } else { Some(pattern) };
                return Ok(ParamOp::Case(upper, all, pattern));
            }
            '~' if !self.options.posix => {
                let all = self.eat("~");
                let pattern = self.brace_word(in_double, true)?;
                let pattern = if pattern.parts.is_empty() { None } else { Some(pattern) };
                return Ok(ParamOp::Toggle(all, pattern));
            }
            '@' if !self.options.posix => {
                let mut text = String::new();
                loop {
                    match self.bump() {
                        None => return Err(self.unmatched_here('}')),
                        Some('}') => break,
                        Some(c) => text.push(c),
                    }
                }
                let mut letters = text.chars();
                return Ok(match (letters.next(), letters.next()) {
                    (Some(t), None) if "QEPAKaUuLk".contains(t) => ParamOp::Transform(t),
                    _ => ParamOp::BadTransform(text),
                });
            }
            _ => return Err(self.bad_substitution()),
        };
        Ok(op)
    }

    /// The text of a substring offset or length, through the `:` or `}`
    /// after it; whether a `:` (a length) follows. A `:` or `}` inside
    /// parentheses, quotes or an expansion such as `${#name}`, or the
    /// `:` of a conditional `?:`, is part of the text.
    fn substring_part(&mut self) -> Result<(String, bool)> {
        let mut text = String::new();
        let (mut parens, mut braces, mut conditionals) = (0usize, 0usize, 0usize);
        let mut quote = None;
        loop {
            let Some(c) = self.bump() else { return Err(self.unmatched_here('}')) };
            if let Some(open) = quote {
                text.push(c);
                if c == open {
                    quote = None;
                } else if c == '\\'
                    && open == '"'
                    && let Some(next) = self.bump()
                {
                    text.push(next);
                }
                continue;
            }
            let top = parens == 0 && braces == 0;
            match c {
                '}' if top => return Ok((text, false)),
                ':' if top && conditionals == 0 => return Ok((text, true)),
                ':' if top => conditionals -= 1,
                '?' if top => conditionals += 1,
                '\\' => {
                    text.push(c);
                    if let Some(next) = self.bump() {
                        text.push(next);
                    }
                    continue;
                }
                '\'' | '"' | '`' => quote = Some(c),
                '$' if self.peek() == Some('{') => {
                    text.push(c);
                    self.bump();
                    text.push('{');
                    braces += 1;
                    continue;
                }
                '}' if braces > 0 => braces -= 1,
                '(' => parens += 1,
                ')' if parens > 0 => parens -= 1,
                _ => {}
            }
            text.push(c);
        }
    }

    /// The pattern of `${name/pattern/replacement}`, through the `/` or
    /// `}` after it; whether a replacement follows.
    fn replace_pattern(&mut self, in_double: bool) -> Result<(Word, bool)> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            let Some(c) = self.peek() else { return Err(self.unmatched_here('}')) };
            match c {
                '/' => {
                    self.bump();
                    flush(&mut lit, &mut parts);
                    return Ok((Word { parts }, true));
                }
                '}' => {
                    self.bump();
                    flush(&mut lit, &mut parts);
                    return Ok((Word { parts }, false));
                }
                _ => self.brace_char(c, in_double, true, &mut lit, &mut parts)?,
            }
        }
    }

    /// The word after an operator of `${...}`, through the `}`. Inside
    /// double quotes, single quotes stand for themselves unless the word
    /// is a pattern.
    fn brace_word(&mut self, in_double: bool, pattern: bool) -> Result<Word> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut depth = 0usize;
        loop {
            let Some(c) = self.peek() else { return Err(self.unmatched_here('}')) };
            match c {
                '}' if depth == 0 => {
                    self.bump();
                    break;
                }
                '{' => {
                    depth += 1;
                    lit.push(c);
                    self.bump();
                }
                '}' => {
                    depth -= 1;
                    lit.push(c);
                    self.bump();
                }
                _ => self.brace_char(c, in_double, pattern, &mut lit, &mut parts)?,
            }
        }
        flush(&mut lit, &mut parts);
        Ok(Word { parts })
    }

    /// One character (or quoted string, or expansion) of a `${...}`
    /// operand.
    fn brace_char(
        &mut self,
        c: char,
        in_double: bool,
        pattern: bool,
        lit: &mut String,
        parts: &mut Vec<WordPart>,
    ) -> Result<()> {
        match c {
            '\\' => {
                self.bump();
                match self.bump_raw() {
                    Some(e)
                        if in_double && !pattern && !matches!(e, '$' | '`' | '"' | '\\' | '}') =>
                    {
                        lit.push('\\');
                        lit.push(e);
                    }
                    Some(e) => {
                        flush(lit, parts);
                        parts.push(WordPart::Escaped(e));
                    }
                    None => return Err(self.unmatched_here('}')),
                }
            }
            '\'' if !in_double || pattern => {
                self.bump();
                flush(lit, parts);
                let mut text = String::new();
                loop {
                    match self.bump_raw() {
                        Some('\'') => break,
                        Some(c) => text.push(c),
                        None => return Err(self.unmatched_here('\'')),
                    }
                }
                parts.push(WordPart::Quoted(text));
            }
            '"' => {
                self.bump();
                flush(lit, parts);
                let inner = self.double_quoted()?;
                parts.push(WordPart::Double(inner));
            }
            '`' => {
                self.bump();
                flush(lit, parts);
                parts.push(self.backquoted(in_double)?);
            }
            '$' => {
                flush(lit, parts);
                match self.dollar(in_double)? {
                    Some(part) => parts.push(part),
                    None => lit.push('$'),
                }
            }
            _ => {
                lit.push(c);
                self.bump();
            }
        }
        Ok(())
    }

    // Text parsed again: substitutions, here-documents, arithmetic.

    /// Parses `text`, the inside of `` `...` ``, as commands.
    fn sub_program(&self, text: &str, line: u32) -> Result<Program> {
        let options = self.options.clone();
        let mut parser = Parser::new(text, self.file.clone(), line, options);
        parser.depth = self.depth + 1;
        parser.parse().map_err(|error| match error {
            Error::Incomplete(unfinished) => {
                Error::Syntax { messages: at_line(unfinished.report(line), line), context: None }
            }
            error => error,
        })
    }

    /// Parses `text` as a word that is read like the inside of double
    /// quotes: a here-document's body, an arithmetic expression, or a
    /// subscript.
    fn sub_word(&self, text: &str, line: u32, mode: WordMode) -> Result<Word> {
        let mut parser = Parser::new(text, self.file.clone(), line, self.options.clone());
        parser.depth = self.depth + 1;
        let mut parts = Vec::new();
        let mut lit = String::new();
        // A subscript's plain text is kept apart from its quoted text, so
        // that it shows as it was written.
        let flush_text = |lit: &mut String, parts: &mut Vec<WordPart>| match mode {
            WordMode::Subscript => flush(lit, parts),
            _ => flush_quoted(lit, parts),
        };
        let incomplete = |error| match error {
            Error::Incomplete(unfinished) => {
                Error::Syntax { messages: at_line(unfinished.report(line), line), context: None }
            }
            error => error,
        };
        while let Some(c) = parser.peek() {
            match c {
                // In a subscript every escape is kept, for an associative
                // array's key loses its backslashes as a word does, and an
                // arithmetic index keeps most.
                '\\' if mode == WordMode::Subscript => {
                    parser.bump();
                    match parser.bump_raw() {
                        Some(e) => {
                            flush_text(&mut lit, &mut parts);
                            parts.push(WordPart::Escaped(e));
                        }
                        None => lit.push('\\'),
                    }
                }
                '"' if mode == WordMode::Subscript => {
                    parser.bump();
                    flush_text(&mut lit, &mut parts);
                    let inner = parser.double_quoted().map_err(incomplete)?;
                    parts.push(WordPart::Double(inner));
                }
                '\\' => {
                    parser.bump();
                    match parser.bump_raw() {
                        Some(e @ ('$' | '`' | '\\')) => lit.push(e),
                        Some('"') if mode == WordMode::Arith => lit.push('"'),
                        Some(e) => {
                            lit.push('\\');
                            lit.push(e);
                        }
                        None => lit.push('\\'),
                    }
                }
                '`' => {
                    parser.bump();
                    flush_text(&mut lit, &mut parts);
                    parts.push(parser.backquoted(false).map_err(incomplete)?);
                }
                '$' => {
                    flush_text(&mut lit, &mut parts);
                    match parser.dollar(true).map_err(incomplete)? {
                        Some(part) => parts.push(part),
                        None => lit.push('$'),
                    }
                }
                '"' if mode == WordMode::Arith => {
                    // Double quotes in arithmetic group nothing.
                    parser.bump();
                }
                '\'' if mode != WordMode::HereDoc => {
                    parser.bump();
                    flush_text(&mut lit, &mut parts);
                    let mut text = String::new();
                    loop {
                        match parser.bump_raw() {
                            Some('\'') => break,
                            Some(c) => text.push(c),
                            None => {
                                return Err(Error::syntax(
                                    line,
                                    "unexpected EOF while looking for matching `''",
                                    None,
                                ));
                            }
                        }
                    }
                    parts.push(WordPart::Quoted(text));
                }
                _ => {
                    lit.push(c);
                    parser.bump_raw();
                }
            }
        }
        flush_text(&mut lit, &mut parts);
        Ok(Word { parts })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordMode {
    HereDoc,
    Arith,
    /// An array's subscript: arithmetic, or an associative array's key.
    Subscript,
}

/// Whether `c` ends a command where a word was wanted.
fn command_ends(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | ')')
}

/// What comes next in `[[ ]]`, as bash's reader takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CondNext {
    End,
    Newline,
    /// `]]`.
    Close,
    /// An operator: `&&`, `(`, `;` and the rest of [`OPERATORS`].
    Op(&'static str),
    Word,
}

impl CondNext {
    /// The token, as bash names it in an error; `None` for a word.
    fn name(self) -> Option<&'static str> {
        match self {
            CondNext::End => Some("EOF"),
            CondNext::Newline => Some("newline"),
            CondNext::Close => Some("]]"),
            CondNext::Op(op) => Some(op),
            CondNext::Word => None,
        }
    }
}

/// The operators, longest first.
const OPERATORS: [&str; 23] = [
    ";;&", "&>>", "<<<", "<<-", "&&", "||", ";;", ";&", "|&", "&>", "<<", "<>", "<&", ">>", ">&",
    ">|", "&", "|", ";", "<", ">", "(", ")",
];

/// The two-operand tests of `[[ ]]` that are words.
const BINARY_TESTS: [&str; 13] =
    ["==", "!=", "=~", "=", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef"];

fn is_unary_test(op: &str) -> bool {
    matches!(
        op,
        "-a" | "-b"
            | "-c"
            | "-d"
            | "-e"
            | "-f"
            | "-g"
            | "-h"
            | "-k"
            | "-p"
            | "-r"
            | "-s"
            | "-t"
            | "-u"
            | "-w"
            | "-x"
            | "-G"
            | "-L"
            | "-N"
            | "-O"
            | "-S"
            | "-z"
            | "-n"
            | "-o"
            | "-v"
            | "-R"
    )
}

/// `messages`, all at `line`: what bash says of text parsed again that
/// ends early, at the line it started on.
fn at_line(messages: Vec<(u32, String)>, line: u32) -> Vec<(u32, String)> {
    messages.into_iter().map(|(_, text)| (line, text)).collect()
}

fn param(name: ParamName) -> WordPart {
    WordPart::Param(Box::new(ParamExp {
        name,
        index: None,
        op: ParamOp::None,
        indirect: false,
        braced: false,
    }))
}

/// Moves unquoted text into a part of its own.
fn flush(lit: &mut String, parts: &mut Vec<WordPart>) {
    if !lit.is_empty() {
        parts.push(WordPart::Lit(std::mem::take(lit)));
    }
}

/// Moves quoted text into a part of its own.
fn flush_quoted(text: &mut String, parts: &mut Vec<WordPart>) {
    if !text.is_empty() {
        parts.push(WordPart::Quoted(std::mem::take(text)));
    }
}

/// Appends the character with code `value`; `byte` for `\x` and octal
/// escapes, whose values above 127 are bytes rather than characters.
fn push_code(out: &mut String, value: u32, byte: bool) {
    if byte {
        out.push(crate::text::byte_char(value as u8));
    } else if let Some(c) = char::from_u32(value) {
        out.push(c);
    }
}

/// The three expressions of `for ((a; b; c))`.
fn split_arith_for(text: &str) -> Option<[String; 3]> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ';' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current);
    let [a, b, c]: [String; 3] = parts.try_into().ok()?;
    Some([a, b, c])
}

/// Parses `text` with the default options: the whole of a script.
pub fn parse(text: &str, file: &str) -> Result<Program> {
    Parser::new(text, file.into(), 1, Options::default()).parse()
}

/// Parses `text` as a word read like the inside of double quotes, which
/// is how a prompt, `PS4` and `${x@P}` are expanded.
pub fn string_word(text: &str, file: Rc<str>, options: Options) -> Result<Word> {
    Parser::new("", file, 1, options).sub_word(text, 1, WordMode::HereDoc)
}

/// Parses `text` as the words of a command, as `compgen -W` and
/// `complete -W` read their word lists.
pub fn words(text: &str, options: Options) -> Result<Vec<Word>> {
    let mut parser = Parser::new(text, Rc::from("compgen"), 1, options);
    let mut words = Vec::new();
    loop {
        parser.skip_blanks();
        while parser.eat("\n") {
            parser.skip_blanks();
        }
        match parser.word(Ctx::Normal)? {
            Some(word) => words.push(word),
            None => match parser.peek() {
                None => return Ok(words),
                Some(c) => return Err(parser.unexpected(c)),
            },
        }
    }
}

#[cfg(test)]
mod tests;
