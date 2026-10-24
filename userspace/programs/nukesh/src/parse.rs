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
    Incomplete(String),
    /// The text is not shell.
    Syntax { line: u32, message: String },
}

impl Error {
    pub fn line(&self) -> Option<u32> {
        match self {
            Error::Incomplete(_) => None,
            Error::Syntax { line, .. } => Some(*line),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Error::Incomplete(message) | Error::Syntax { message, .. } => message,
        }
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
            result = Err(self.incomplete("a continued line"));
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
            self.skip_blanks();
            let sep = match self.peek() {
                Some('&') if !self.at("&&") => {
                    self.pos += 1;
                    Sep::Async
                }
                Some(';') if !self.at(";;") && !self.at(";&") => {
                    self.pos += 1;
                    Sep::Seq
                }
                _ => Sep::Seq,
            };
            command.items.push((item, sep));
            self.skip_blanks();
            match self.peek() {
                None => break,
                Some('\n') => {
                    self.newline()?;
                    break;
                }
                Some(c) if command_ends(c) => return Err(self.unexpected(c)),
                Some(_) => {}
            }
        }
        Ok(Some(command))
    }

    /// The line the parser is at.
    pub fn line(&self) -> u32 {
        self.line
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
        Error::Syntax { line: self.line, message: message.into() }
    }

    fn incomplete(&self, what: &str) -> Error {
        Error::Incomplete(format!("{what} is not finished"))
    }

    /// The error for something the grammar does not allow here.
    fn unexpected(&mut self, c: char) -> Error {
        let what = if c == '\n' {
            "newline".to_string()
        } else {
            let mut token = String::new();
            let mut at = self.pos;
            while let Some(&c) = self.text.get(at) {
                if is_meta(c) && !token.is_empty() || c == '\n' || token.len() > 20 {
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
            format!("`{token}`")
        };
        self.syntax(format!("unexpected {what}"))
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
                self.skip_blanks();
                let sep = match self.peek() {
                    Some('&') if !self.at("&&") && !self.at("&>") => {
                        self.pos += 1;
                        Sep::Async
                    }
                    Some(';') if !self.at(";;") && !self.at(";&") => {
                        self.pos += 1;
                        Sep::Seq
                    }
                    _ => Sep::Seq,
                };
                command.items.push((item, sep));
                self.skip_blanks();
                match self.peek() {
                    None => break,
                    Some('\n') => break,
                    _ if self.at_end(end) => break,
                    Some(c) if command_ends(c) => return Err(self.unexpected(c)),
                    Some(_) => {}
                }
            }
            program.commands.push(command);
        }
    }

    /// A list that must have a command: the body of a compound command.
    fn compound_list(&mut self, end: End) -> Result<Program> {
        let program = self.list(end)?;
        if program.commands.is_empty() {
            return Err(match self.peek() {
                None => self.incomplete("a command list"),
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
                return Err(self.incomplete("a `&&` or `||` list"));
            }
            rest.push((op, self.pipeline()?));
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline> {
        self.skip_blanks();
        let line = self.line;
        let mut time = None;
        if self.at_word("time") {
            self.eat("time");
            self.skip_blanks();
            let posix_format = self.at_word("-p");
            if posix_format {
                self.eat("-p");
            }
            time = Some(posix_format);
            self.skip_blanks();
        }
        let mut bang = false;
        while self.at_word("!") {
            self.eat("!");
            bang = !bang;
            self.skip_blanks();
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
                return Err(self.incomplete("a pipeline"));
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
            None => return Err(self.incomplete("a command")),
            Some('(') if self.at("((") && !self.options.posix => match self.arith_command()? {
                Some(compound) => compound,
                None => self.subshell()?,
            },
            Some('(') => self.subshell()?,
            Some(c) if command_ends(c) => return Err(self.unexpected(c)),
            Some(_) => {
                if self.eat_word("{") {
                    let body = self.compound_list(End::Words(&["}"]))?;
                    self.expect_word("}", "`{`")?;
                    CompoundCommand::Brace(body)
                } else if self.eat_word("if") {
                    self.if_command()?
                } else if self.eat_word("while") {
                    let cond = self.compound_list(End::Words(&["do"]))?;
                    let body = self.do_group("`while`")?;
                    CompoundCommand::While { cond, body }
                } else if self.eat_word("until") {
                    let cond = self.compound_list(End::Words(&["do"]))?;
                    let body = self.do_group("`until`")?;
                    CompoundCommand::Until { cond, body }
                } else if self.eat_word("for") {
                    self.for_command(line)?
                } else if self.eat_word("case") {
                    self.case_command(line)?
                } else if !self.options.posix && self.eat_word("select") {
                    self.select_command(line)?
                } else if !self.options.posix && self.eat_word("[[") {
                    let expr = self.cond_or()?;
                    self.skip_cond_blanks()?;
                    if !self.eat_word("]]") {
                        return match self.peek() {
                            None => Err(self.incomplete("`[[`")),
                            Some(c) => Err(self.unexpected(c)),
                        };
                    }
                    CompoundCommand::Cond(expr, line)
                } else if !self.options.posix && self.eat_word("function") {
                    return self.function_keyword(line);
                } else {
                    for word in ["}", "do", "done", "elif", "else", "esac", "fi", "then", "]]"] {
                        if self.at_word(word) {
                            return Err(self.syntax(format!("unexpected `{word}`")));
                        }
                    }
                    return self.simple_command(line);
                }
            }
        };
        let redirects = self.redirects()?;
        Ok(Command::Compound(Box::new(compound), redirects))
    }

    fn expect_word(&mut self, word: &str, opened: &str) -> Result<()> {
        self.linebreak()?;
        if self.eat_word(word) {
            return Ok(());
        }
        match self.peek() {
            None => Err(self.incomplete(opened)),
            Some(c) => {
                let mut error = self.unexpected(c);
                if let Error::Syntax { message, .. } = &mut error {
                    message.push_str(&format!(", wanted `{word}` to end {opened}"));
                }
                Err(error)
            }
        }
    }

    fn subshell(&mut self) -> Result<CompoundCommand> {
        self.bump();
        let body = self.compound_list(End::Paren)?;
        self.linebreak()?;
        if !self.eat(")") {
            return match self.peek() {
                None => Err(self.incomplete("a subshell")),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        if body.commands.is_empty() {
            return Err(self.syntax("unexpected `)`: a subshell needs a command"));
        }
        Ok(CompoundCommand::Subshell(body))
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
        let mut text = String::new();
        let mut depth = 0usize;
        loop {
            let Some(c) = self.bump() else { return Err(self.incomplete("`((`")) };
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
                    loop {
                        let Some(q) = self.bump_raw() else {
                            return Err(self.incomplete("a quoted string"));
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
            self.expect_word("then", "`if`")?;
            let body = self.compound_list(End::Words(&["elif", "else", "fi"]))?;
            arms.push((cond, body));
            self.linebreak()?;
            if self.eat_word("elif") {
                continue;
            }
            if self.eat_word("else") {
                otherwise = Some(self.compound_list(End::Words(&["fi"]))?);
            }
            self.expect_word("fi", "`if`")?;
            return Ok(CompoundCommand::If { arms, otherwise });
        }
    }

    fn do_group(&mut self, opened: &str) -> Result<Program> {
        self.expect_word("do", opened)?;
        let body = self.compound_list(End::Words(&["done"]))?;
        self.expect_word("done", opened)?;
        Ok(body)
    }

    /// The name after `for` or `select`.
    fn loop_name(&mut self, what: &str) -> Result<String> {
        self.skip_blanks();
        let start = self.pos;
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if !is_name_char(c) {
                break;
            }
            name.push(c);
            self.bump();
        }
        if !is_name(&name) {
            self.pos = start;
            return match self.peek() {
                None => Err(self.incomplete(what)),
                Some(c) => {
                    let mut error = self.unexpected(c);
                    if let Error::Syntax { message, .. } = &mut error {
                        message.push_str(&format!(", wanted a name after {what}"));
                    }
                    Err(error)
                }
            };
        }
        Ok(name)
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
                None => return Err(self.incomplete("`for`")),
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
                return Err(self.syntax("`for ((` needs `))`"));
            };
            let parts = split_arith_for(&text)
                .ok_or_else(|| self.syntax("`for ((...))` needs three expressions"))?;
            let init = self.sub_word(&parts[0], line, WordMode::Arith)?;
            let cond = self.sub_word(&parts[1], line, WordMode::Arith)?;
            let step = self.sub_word(&parts[2], line, WordMode::Arith)?;
            self.skip_blanks();
            self.eat(";");
            let body = self.loop_body("`for`")?;
            return Ok(CompoundCommand::ArithFor { init, cond, step, body, line });
        }
        let var = self.loop_name("`for`")?;
        let words = self.loop_words()?;
        let body = self.do_group("`for`")?;
        Ok(CompoundCommand::For { var, words, body, line })
    }

    /// `do list done`, or bash's `{ list }` after `for ((...))`.
    fn loop_body(&mut self, opened: &str) -> Result<Program> {
        self.linebreak()?;
        if self.eat_word("{") {
            let body = self.compound_list(End::Words(&["}"]))?;
            self.expect_word("}", opened)?;
            return Ok(body);
        }
        self.do_group(opened)
    }

    fn select_command(&mut self, line: u32) -> Result<CompoundCommand> {
        let var = self.loop_name("`select`")?;
        let words = self.loop_words()?;
        let body = self.do_group("`select`")?;
        Ok(CompoundCommand::Select { var, words, body, line })
    }

    fn case_command(&mut self, line: u32) -> Result<CompoundCommand> {
        self.skip_blanks();
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.incomplete("`case`")),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        self.linebreak()?;
        if !self.eat_word("in") {
            return match self.peek() {
                None => Err(self.incomplete("`case`")),
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
                return Err(self.incomplete("`case`"));
            }
            self.eat("(");
            let mut patterns = Vec::new();
            loop {
                self.skip_blanks();
                match self.word(Ctx::Normal)? {
                    Some(pattern) => patterns.push(pattern),
                    None => {
                        return match self.peek() {
                            None => Err(self.incomplete("`case`")),
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
                    None => Err(self.incomplete("`case`")),
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
                    None => Err(self.incomplete("`case`")),
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
                None => Err(self.incomplete("`function`")),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        let Some(name) = name_word.as_literal().map(str::to_string) else {
            return Err(self.syntax("a function's name must be a plain word"));
        };
        self.skip_blanks();
        if self.at("(") {
            self.bump();
            self.skip_blanks();
            if !self.eat(")") {
                return match self.peek() {
                    None => Err(self.incomplete("`function`")),
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
            return Err(self.incomplete("a function definition"));
        }
        let body = self.command()?;
        if !matches!(body, Command::Compound(..)) {
            return Err(self.syntax(format!("the body of {name} must be a compound command")));
        }
        Ok(Command::Function(Rc::new(FunctionDef { name, body, file: self.file.clone(), line })))
    }

    fn simple_command(&mut self, line: u32) -> Result<Command> {
        let mut command = SimpleCommand { line, ..SimpleCommand::default() };
        loop {
            self.skip_blanks();
            if let Some(redirect) = self.redirect()? {
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
            if (command.words.is_empty() || declaration)
                && let Some(assign) = self.assignment()?
            {
                if command.words.is_empty() {
                    command.assigns.push(assign);
                } else {
                    command
                        .words
                        .push(Word { parts: vec![WordPart::ArrayAssign(Box::new(assign))] });
                }
                continue;
            }
            let Some(word) = self.word(Ctx::Normal)? else { break };
            command.words.push(word);
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
                            return Err(self.syntax(format!("`{name}` is a reserved word")));
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
                None => Err(self.incomplete("a command")),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        Ok(Command::Simple(command))
    }

    /// `name=value`, `name+=value`, `name[sub]=value` or `name=(...)`, if
    /// the next word is one; otherwise nothing is consumed.
    fn assignment(&mut self) -> Result<Option<Assign>> {
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
            let mut text = String::new();
            let mut depth = 0;
            loop {
                match self.peek() {
                    Some(']') if depth == 0 => {
                        self.bump();
                        break;
                    }
                    // A quoted subscript, as in `m["a key"]=1`, may hold
                    // blanks: up to the closing quote whatever it holds.
                    Some(quote @ ('\'' | '"')) => {
                        text.push(quote);
                        self.bump();
                        loop {
                            match self.peek() {
                                None => {
                                    (self.pos, self.line) = (start, start_line);
                                    return Ok(None);
                                }
                                Some('\\') if quote == '"' => {
                                    text.push('\\');
                                    self.bump();
                                    if let Some(c) = self.peek() {
                                        text.push(c);
                                        self.bump();
                                    }
                                }
                                Some(c) => {
                                    text.push(c);
                                    self.bump();
                                    if c == quote {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some(c) if c == '\n' || c == ' ' || c == '\t' || c == ';' || c == '&' => {
                        (self.pos, self.line) = (start, start_line);
                        return Ok(None);
                    }
                    Some(c) => {
                        if c == '[' {
                            depth += 1;
                        } else if c == ']' {
                            depth -= 1;
                        }
                        text.push(c);
                        self.bump();
                    }
                    None => {
                        (self.pos, self.line) = (start, start_line);
                        return Ok(None);
                    }
                }
            }
            index = Some(self.sub_word(&text, self.line, WordMode::Arith)?);
        }
        let append = !self.options.posix && self.eat("+=");
        if !append && !self.eat("=") {
            (self.pos, self.line) = (start, start_line);
            return Ok(None);
        }
        if !self.options.posix && index.is_none() && self.peek() == Some('(') {
            self.bump();
            let mut elements = Vec::new();
            loop {
                self.linebreak()?;
                match self.peek() {
                    None => return Err(self.incomplete("an array assignment")),
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
                    let mut text = String::new();
                    let mut closed = false;
                    while let Some(c) = self.peek() {
                        self.bump();
                        if c == ']' {
                            closed = true;
                            break;
                        }
                        text.push(c);
                    }
                    if closed && self.eat("=") {
                        key = Some(self.sub_word(&text, self.line, WordMode::Arith)?);
                    } else {
                        (self.pos, self.line) = save;
                    }
                }
                match self.word(Ctx::Normal)? {
                    Some(word) => elements.push((key, word)),
                    None if key.is_some() => elements.push((key, Word::default())),
                    None => match self.peek() {
                        None => return Err(self.incomplete("an array assignment")),
                        Some(c) => return Err(self.unexpected(c)),
                    },
                }
            }
            return Ok(Some(Assign { name, index, append, value: AssignValue::Array(elements) }));
        }
        let value = self.word(Ctx::Normal)?.unwrap_or_default();
        Ok(Some(Assign { name, index, append, value: AssignValue::Scalar(value) }))
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
            if is_name(&name) && self.eat("}") && matches!(self.peek(), Some('<' | '>')) {
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
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.incomplete("a redirection")),
                Some(c) => {
                    let mut error = self.unexpected(c);
                    if let Error::Syntax { message, .. } = &mut error {
                        message.push_str(", wanted a file name after the redirection");
                    }
                    Err(error)
                }
            };
        };
        Ok(Some(Redirect { fd, op, target: RedirTarget::Word(word) }))
    }

    /// `<<` or `<<-` and its delimiter; the body is read after the line.
    fn heredoc(&mut self, fd: Option<RedirFd>, strip_tabs: bool) -> Result<Redirect> {
        self.skip_blanks();
        let Some(word) = self.word(Ctx::Normal)? else {
            return match self.peek() {
                None => Err(self.incomplete("a here-document")),
                Some(c) => Err(self.unexpected(c)),
            };
        };
        let expand = !word.has_quotes();
        let delimiter = word.unquoted_text();
        let doc = Rc::new(RefCell::new(HereDoc { body: Word::default(), expand }));
        self.pending.push(Pending { doc: doc.clone(), delimiter, strip_tabs });
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
                    // As bash does, the end of the text ends it too, but a
                    // prompt reads on.
                    return Err(Error::Incomplete(format!(
                        "the here-document ending with `{}` is not finished",
                        doc.delimiter
                    )));
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

    /// At the end of the text: a here-document still waiting for its body
    /// is incomplete.
    fn finish_heredocs(&mut self) -> Result<()> {
        if let Some(doc) = self.pending.first() {
            return Err(Error::Incomplete(format!(
                "the here-document ending with `{}` is not finished",
                doc.delimiter
            )));
        }
        Ok(())
    }

    // `[[ ]]`.

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
        loop {
            self.skip_cond_blanks()?;
            if !self.eat("||") {
                return Ok(left);
            }
            let right = self.cond_and()?;
            left = CondExpr::Or(Box::new(left), Box::new(right));
        }
    }

    fn cond_and(&mut self) -> Result<CondExpr> {
        let mut left = self.cond_not()?;
        loop {
            self.skip_cond_blanks()?;
            if !self.eat("&&") {
                return Ok(left);
            }
            let right = self.cond_not()?;
            left = CondExpr::And(Box::new(left), Box::new(right));
        }
    }

    fn cond_not(&mut self) -> Result<CondExpr> {
        self.nested(Parser::cond_not_here)
    }

    fn cond_not_here(&mut self) -> Result<CondExpr> {
        self.skip_cond_blanks()?;
        if self.at_word("!") {
            self.eat("!");
            return Ok(CondExpr::Not(Box::new(self.cond_not()?)));
        }
        if self.eat("(") {
            let inner = self.cond_or()?;
            self.skip_cond_blanks()?;
            if !self.eat(")") {
                return match self.peek() {
                    None => Err(self.incomplete("`[[`")),
                    Some(c) => Err(self.unexpected(c)),
                };
            }
            return Ok(inner);
        }
        self.cond_primary()
    }

    fn cond_word(&mut self) -> Result<Word> {
        self.skip_cond_blanks()?;
        match self.word(Ctx::Cond)? {
            Some(word) => Ok(word),
            None => match self.peek() {
                None => Err(self.incomplete("`[[`")),
                Some(c) => {
                    let mut error = self.unexpected(c);
                    if let Error::Syntax { message, .. } = &mut error {
                        message.push_str(" in `[[`");
                    }
                    Err(error)
                }
            },
        }
    }

    fn cond_primary(&mut self) -> Result<CondExpr> {
        self.skip_cond_blanks()?;
        if self.at_word("]]") {
            return Err(self.syntax("unexpected `]]`: `[[` needs an expression"));
        }
        let first = self.cond_word()?;
        if let Some(op) = first.as_literal()
            && is_unary_test(op)
        {
            // `-f file`, unless the word is itself an operand: `[[ -f ]]`
            // and `[[ -f == x ]]`.
            self.skip_blanks();
            let next_ends = self.peek().is_none_or(|c| matches!(c, '\n' | ')'))
                || self.at_word("]]")
                || self.at("&&")
                || self.at("||");
            if !next_ends && !self.at_binary_op() {
                let operand = self.cond_word()?;
                return Ok(CondExpr::Unary(op.to_string(), operand));
            }
        }
        self.skip_blanks();
        if let Some(op) = self.binary_op() {
            self.skip_cond_blanks()?;
            let right = if op == "=~" {
                match self.word(Ctx::Regex)? {
                    Some(word) => word,
                    None => return Err(self.syntax("`=~` needs a regular expression")),
                }
            } else if matches!(op.as_str(), "==" | "!=" | "=") {
                // A pattern, read as with extglob on, as bash reads it.
                let extglob = std::mem::replace(&mut self.options.extglob, true);
                let word = self.cond_word();
                self.options.extglob = extglob;
                word?
            } else {
                self.cond_word()?
            };
            return Ok(CondExpr::Binary(first, op, right));
        }
        Ok(CondExpr::Word(first))
    }

    /// Whether a binary test operator is next, without consuming it.
    fn at_binary_op(&mut self) -> bool {
        let (pos, line) = (self.pos, self.line);
        let found = self.binary_op().is_some();
        (self.pos, self.line) = (pos, line);
        found
    }

    fn binary_op(&mut self) -> Option<String> {
        for op in ["==", "!=", "=~", "="] {
            if self.at_word(op) {
                self.eat(op);
                return Some(op.to_string());
            }
        }
        if self.at("<") && !self.at("<<") {
            self.bump();
            return Some("<".into());
        }
        if self.at(">") && !self.at(">>") {
            self.bump();
            return Some(">".into());
        }
        for op in ["-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef"] {
            if self.at_word(op) {
                self.eat(op);
                return Some(op.to_string());
            }
        }
        None
    }

    // Words.

    /// Reads a word; `None` if one does not start here.
    fn word(&mut self, ctx: Ctx) -> Result<Option<Word>> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut paren_depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                '(' if ctx == Ctx::Regex => {
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
                ' ' | '\t' if ctx == Ctx::Regex && paren_depth > 0 => {
                    lit.push(c);
                    self.bump();
                }
                '<' | '>' if lit.is_empty() && parts.is_empty() && !self.options.posix => {
                    // `<(...)` and `>(...)`.
                    if self.peek2() == Some('(') && ctx == Ctx::Normal {
                        let output = c == '>';
                        self.bump();
                        self.bump();
                        let program = self.list(End::Paren)?;
                        self.linebreak()?;
                        if !self.eat(")") {
                            return match self.peek() {
                                None => Err(self.incomplete("a process substitution")),
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
                    self.bump();
                    flush(&mut lit, &mut parts);
                    let mut text = String::new();
                    loop {
                        match self.bump_raw() {
                            Some('\'') => break,
                            Some(c) => text.push(c),
                            None => return Err(self.incomplete("a '...' string")),
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
                    lit.push(c);
                    self.bump();
                    self.bump();
                    lit.push('(');
                    let mut depth = 1;
                    while depth > 0 {
                        match self.bump() {
                            None => return Err(self.incomplete("an extended pattern")),
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
        flush(&mut lit, &mut parts);
        if parts.is_empty() { Ok(None) } else { Ok(Some(Word { parts })) }
    }

    /// The inside of `"..."`, after the opening quote, through the
    /// closing one.
    fn double_quoted(&mut self) -> Result<Vec<WordPart>> {
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            let Some(c) = self.peek() else { return Err(self.incomplete("a \"...\" string")) };
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
                        None => return Err(self.incomplete("a \"...\" string")),
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
                None => return Err(self.incomplete("a `...` command substitution")),
                Some('`') => break,
                Some('\\') => match self.bump_raw() {
                    Some(c @ ('$' | '`' | '\\')) => text.push(c),
                    Some('"') if in_double => text.push('"'),
                    Some('\n') => {}
                    Some(c) => {
                        text.push('\\');
                        text.push(c);
                    }
                    None => return Err(self.incomplete("a `...` command substitution")),
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
                None => Err(self.incomplete("a $(...) command substitution")),
                Some(c) => Err(self.unexpected(c)),
            };
        }
        Ok(Some(WordPart::Command(Rc::new(program))))
    }

    /// `$'...'`, after the quote: its escapes decoded.
    fn ansi_c(&mut self) -> Result<String> {
        let mut out = String::new();
        loop {
            let Some(c) = self.bump_raw() else { return Err(self.incomplete("a $'...' string")) };
            match c {
                '\'' => return Ok(out),
                '\\' => {
                    let Some(e) = self.bump_raw() else {
                        return Err(self.incomplete("a $'...' string"));
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
            Err(Error::Syntax { message, .. }) if message.ends_with("bad substitution") => {
                self.pos = pos;
                self.line = line;
                self.depth = depth;
                self.pending.truncate(pending);
                let text = self.brace_text(in_double)?;
                let name = ParamName::Special('?');
                Ok(ParamExp { name, index: None, op: ParamOp::Bad(text), indirect: false })
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
            let Some(c) = self.bump() else { return Err(self.incomplete("a ${...} expansion")) };
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
        let unclosed = |p: &Parser| p.incomplete("a ${...} expansion");
        let mut length = false;
        let mut indirect = false;
        // `${#}` is the count of arguments; `${#name}` a length.
        if self.peek() == Some('#') && !matches!(self.peek2(), Some('}')) {
            let after = self.peek2();
            if after.is_some_and(|c| is_name_char(c) || "@*#?-$!".contains(c)) {
                self.bump();
                length = true;
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
            let mut text = String::new();
            let mut depth = 0;
            loop {
                match self.bump() {
                    None => return Err(unclosed(self)),
                    Some(']') if depth == 0 => break,
                    Some(c) => {
                        if c == '[' {
                            depth += 1;
                        } else if c == ']' {
                            depth -= 1;
                        }
                        text.push(c);
                    }
                }
            }
            index = Some(if text == "@" || text == "*" {
                Word::lit(&text)
            } else {
                self.sub_word(&text, line, WordMode::Arith)?
            });
        }
        if length {
            if !self.eat("}") {
                return Err(self.bad_substitution());
            }
            return Ok(ParamExp { name, index, op: ParamOp::Length, indirect: false });
        }
        if indirect {
            // `${!prefix*}`, `${!prefix@}` and `${!name[@]}`.
            if index.is_none() && (self.at("*}") || self.at("@}")) {
                let all = self.at("@}");
                self.bump();
                self.bump();
                return Ok(ParamExp { name, index, op: ParamOp::Names(all), indirect: false });
            }
            if let Some(sub) = index.as_ref().and_then(Word::as_literal)
                && (sub == "@" || sub == "*")
                && self.peek() == Some('}')
            {
                let all = sub == "@";
                self.bump();
                return Ok(ParamExp { name, index: None, op: ParamOp::Keys(all), indirect: false });
            }
        }
        let op = self.param_op(in_double)?;
        Ok(ParamExp { name, index, op, indirect })
    }

    fn bad_substitution(&mut self) -> Error {
        // Skip to the closing brace, so that the message can name it all.
        self.syntax("bad substitution")
    }

    /// The operator and operands of `${name...}`, through the `}`.
    fn param_op(&mut self, in_double: bool) -> Result<ParamOp> {
        let Some(c) = self.bump() else { return Err(self.incomplete("a ${...} expansion")) };
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
                let Some(t) = self.bump() else {
                    return Err(self.incomplete("a ${...} expansion"));
                };
                if !"QEPAKaUuLk".contains(t) || !self.eat("}") {
                    return Err(self.bad_substitution());
                }
                return Ok(ParamOp::Transform(t));
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
            let Some(c) = self.bump() else { return Err(self.incomplete("a ${...} expansion")) };
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
            let Some(c) = self.peek() else { return Err(self.incomplete("a ${...} expansion")) };
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
            let Some(c) = self.peek() else { return Err(self.incomplete("a ${...} expansion")) };
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
                    None => return Err(self.incomplete("a ${...} expansion")),
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
                        None => return Err(self.incomplete("a '...' string")),
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
            Error::Incomplete(message) => Error::Syntax { line, message },
            error => error,
        })
    }

    /// Parses `text` as a word that is read like the inside of double
    /// quotes: a here-document's body, or an arithmetic expression.
    fn sub_word(&self, text: &str, line: u32, mode: WordMode) -> Result<Word> {
        let mut parser = Parser::new(text, self.file.clone(), line, self.options.clone());
        parser.depth = self.depth + 1;
        let mut parts = Vec::new();
        let mut lit = String::new();
        let incomplete = |error| match error {
            Error::Incomplete(message) => Error::Syntax { line, message },
            error => error,
        };
        while let Some(c) = parser.peek() {
            match c {
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
                    flush_quoted(&mut lit, &mut parts);
                    parts.push(parser.backquoted(false).map_err(incomplete)?);
                }
                '$' => {
                    flush_quoted(&mut lit, &mut parts);
                    match parser.dollar(true).map_err(incomplete)? {
                        Some(part) => parts.push(part),
                        None => lit.push('$'),
                    }
                }
                '"' if mode == WordMode::Arith => {
                    // Double quotes in arithmetic group nothing.
                    parser.bump();
                }
                '\'' if mode == WordMode::Arith => {
                    parser.bump();
                    flush_quoted(&mut lit, &mut parts);
                    let mut text = String::new();
                    loop {
                        match parser.bump_raw() {
                            Some('\'') => break,
                            Some(c) => text.push(c),
                            None => {
                                return Err(Error::Syntax {
                                    line,
                                    message: "a '...' string is not finished".into(),
                                });
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
        flush_quoted(&mut lit, &mut parts);
        Ok(Word { parts })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordMode {
    HereDoc,
    Arith,
}

/// Whether `c` ends a command where a word was wanted.
fn command_ends(c: char) -> bool {
    matches!(c, ';' | '&' | '|' | ')')
}

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

fn param(name: ParamName) -> WordPart {
    WordPart::Param(Box::new(ParamExp { name, index: None, op: ParamOp::None, indirect: false }))
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
