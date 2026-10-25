// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Word expansion (XCU 2.6): brace expansion, then tilde, parameter,
//! command and arithmetic expansion from left to right, then field
//! splitting, pathname expansion and quote removal.
//!
//! A word is first turned into pieces that remember how each bit of text
//! came: written unquoted, quoted, or the unquoted result of an
//! expansion. Only the last are split at `IFS`; only unquoted text can
//! glob; and the boundaries between the elements of `"$@"` always start a
//! new field. Quote removal then needs no work of its own: the pieces hold
//! the text without its quotes.

use crate::ast::{ParamExp, ParamName, ParamOp, ReplaceKind, Word, WordPart};
use crate::glob::{self, GlobOptions};
use crate::pattern::{self, Options, Pattern};
use crate::shell::{Flow, Shell};
use crate::vars::{self, Value, attr};
use crate::{brace, sys, text};

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    /// Unquoted text as written: it can glob, but is not split.
    Lit(String),
    /// Quoted text, or the result of a quoted expansion.
    Quoted(String),
    /// The result of an unquoted expansion: split, and can glob.
    Split(String),
    /// Between two elements of `"$@"`: always ends the field.
    Break,
    /// Between two elements of an unquoted `$@` or `$*`: ends a field that
    /// has begun. Where nothing is split, as in an assignment, the text
    /// joins them: a space for `$@`, the first character of `IFS` for `$*`.
    SoftBreak(String),
}

/// A field after splitting: its text in segments, each quoted or not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    segs: Vec<(String, bool)>,
}

impl Field {
    fn push(&mut self, text: &str, quoted: bool) {
        match self.segs.last_mut() {
            Some((last, q)) if *q == quoted => last.push_str(text),
            _ => self.segs.push((text.to_string(), quoted)),
        }
    }

    pub fn text(&self) -> String {
        self.segs.iter().map(|(s, _)| s.as_str()).collect()
    }

    /// The text with each quoted character for which `special` is true
    /// behind a backslash.
    fn escaped(&self, special: impl Fn(char) -> bool) -> String {
        let mut out = String::new();
        for (text, quoted) in &self.segs {
            if *quoted {
                for c in text.chars() {
                    if special(c) {
                        out.push('\\');
                    }
                    out.push(c);
                }
            } else {
                out.push_str(text);
            }
        }
        out
    }

    /// The field as a pattern, quoted characters taken literally.
    pub fn pattern(&self) -> String {
        self.escaped(|c| "\\*?[]()|!@+^".contains(c))
    }

    /// The field as an extended regular expression, quoted characters
    /// taken literally.
    pub fn regex(&self) -> String {
        self.escaped(|c| "\\.[]()*+?{}|^$".contains(c))
    }
}

/// How a parameter expanded, before quoting decides what it becomes.
#[derive(Clone, Debug, PartialEq)]
enum PVal {
    Unset,
    One(String),
    /// `$@`, `${a[@]}` and their kin; `star` for `$*` and `${a[*]}`,
    /// which join when quoted.
    List(Vec<String>, bool),
}

impl PVal {
    /// Unset, or with `colon`, unset or empty, as `${x:-...}` asks.
    fn missing(&self, colon: bool) -> bool {
        match self {
            PVal::Unset => true,
            PVal::One(s) => colon && s.is_empty(),
            PVal::List(items, _) => {
                items.is_empty() || colon && items.iter().all(String::is_empty) && items.len() <= 1
            }
        }
    }

    fn map(self, mut f: impl FnMut(String) -> String) -> PVal {
        match self {
            PVal::Unset => PVal::Unset,
            PVal::One(s) => PVal::One(f(s)),
            PVal::List(items, star) => PVal::List(items.into_iter().map(f).collect(), star),
        }
    }
}

/// Whether a parameter expansion gives a list of fields: `$@`, `${a[@]}`
/// and the like.
fn is_list(p: &ParamExp) -> bool {
    let all = matches!(p.name, ParamName::Special('@' | '*'))
        || p.index.as_ref().and_then(Word::as_literal).is_some_and(|i| i == "@" || i == "*");
    match p.op {
        ParamOp::Length => false,
        ParamOp::Keys(_) | ParamOp::Names(_) => true,
        _ => all && !p.indirect,
    }
}

/// Whether `parts` are a lone `$@`-like expansion, which inside double
/// quotes makes no field at all when it is empty.
fn only_list(parts: &[WordPart]) -> bool {
    matches!(parts, [WordPart::Param(p)] if is_list(p))
}

impl Shell {
    /// The fields `words` expand to, as the arguments of a command.
    pub fn expand_words(&mut self, words: &[Word]) -> Result<Vec<String>, Flow> {
        let mut out = Vec::with_capacity(words.len());
        for word in words {
            // Plain words, the most common, need none of the work.
            if let Some(text) = word.as_literal()
                && !text.starts_with('~')
                && !text.contains('{')
                && !pattern::has_meta(text, self.shopt("extglob"))
            {
                out.push(text.to_string());
                continue;
            }
            if self.opts.braceexpand {
                for word in brace::expand(word) {
                    self.expand_into(&word, &mut out)?;
                }
            } else {
                self.expand_into(word, &mut out)?;
            }
        }
        Ok(out)
    }

    fn expand_into(&mut self, word: &Word, out: &mut Vec<String>) -> Result<(), Flow> {
        let pieces = self.pieces(word, true)?;
        for field in self.split(pieces) {
            self.glob_field(&field, out)?;
        }
        Ok(())
    }

    /// Pathname expansion of one field.
    fn glob_field(&mut self, field: &Field, out: &mut Vec<String>) -> Result<(), Flow> {
        if !self.opts.noglob {
            let pattern = field.pattern();
            if pattern::has_meta(&pattern, self.shopt("extglob")) {
                let options = GlobOptions {
                    extglob: self.shopt("extglob"),
                    nocase: self.shopt("nocaseglob"),
                    dotglob: self.shopt("dotglob"),
                    globstar: self.shopt("globstar"),
                    // `.*` takes in `.` and `..` in POSIX mode, as in dash.
                    dots: self.posix() || !self.shopt("globskipdots"),
                };
                let matches = glob::glob(&pattern, options);
                if !matches.is_empty() {
                    out.extend(matches);
                    return Ok(());
                }
                if self.shopt("nullglob") {
                    return Ok(());
                }
                if self.shopt("failglob") {
                    return Err(self.discard(&format!("no match: {}", field.text())));
                }
            }
        }
        out.push(field.text());
        Ok(())
    }

    /// A word expanded to one string, without splitting or globbing: an
    /// assignment's value, a redirection's file, a `case` word.
    pub fn expand_string(&mut self, word: &Word) -> Result<String, Flow> {
        if let Some(text) = word.as_literal()
            && !text.starts_with('~')
        {
            return Ok(text.to_string());
        }
        let pieces = self.pieces(word, true)?;
        Ok(join(&pieces))
    }

    /// A redirection's file or descriptor: the one word it expands to, or
    /// `None` for none or more than one, which is an ambiguous redirect.
    /// Outside POSIX mode it is split and globbed as an argument is, as
    /// bash does; in POSIX mode neither happens, but in an interactive
    /// shell a pattern still globs, and as in bash, a brace still expands
    /// and an unquoted expansion that comes to nothing makes no word.
    pub fn redirect_word(&mut self, word: &Word) -> Result<Option<String>, Flow> {
        let mut fields = Vec::new();
        if !self.posix() {
            fields = self.expand_words(std::slice::from_ref(word))?;
        } else {
            let words =
                if self.opts.braceexpand { brace::expand(word) } else { vec![word.clone()] };
            if words.len() != 1 {
                return Ok(None);
            }
            let field = one_field(self.pieces(&words[0], true)?);
            if field.segs.iter().all(|(text, quoted)| !quoted && text.is_empty()) {
                return Ok(None);
            }
            if self.interactive {
                self.glob_field(&field, &mut fields)?;
            } else {
                fields.push(field.text());
            }
        }
        Ok(match <[String; 1]>::try_from(fields) {
            Ok([text]) => Some(text),
            Err(_) => None,
        })
    }

    /// The subscript of an element named in text (`unset 'a[$i]'`,
    /// `printf -v 'a[$i]'`, `{a[$i]}>file`) with its expansions done, as
    /// bash does them before the arithmetic or the key: parameters,
    /// commands and arithmetic, as in double quotes.
    pub fn expand_index(&mut self, index: &str) -> Result<String, Flow> {
        if !index.contains(['$', '`']) {
            return Ok(index.to_string());
        }
        match crate::parse::string_word(index, self.source.clone(), self.parse_options()) {
            Ok(word) => self.expand_quoted(&word),
            Err(_) => Ok(index.to_string()),
        }
    }

    /// `name` or `name[subscript]` as text, its subscript expanded (see
    /// [`Shell::expand_index`]).
    pub fn expand_element(&mut self, name: &str) -> Result<String, Flow> {
        match name.split_once('[') {
            Some((base, rest)) if rest.ends_with(']') && rest.contains(['$', '`']) => {
                Ok(format!("{base}[{}]", self.expand_index(&rest[..rest.len() - 1])?))
            }
            _ => Ok(name.to_string()),
        }
    }

    /// An assignment's value: as [`Shell::expand_string`], with a tilde
    /// expanded after each unquoted `:` too.
    pub fn expand_assignment(&mut self, word: &Word) -> Result<String, Flow> {
        let mut pieces = Vec::new();
        for (i, part) in word.parts.iter().enumerate() {
            match part {
                WordPart::Lit(s) if s.contains('~') => {
                    let last = i + 1 == word.parts.len();
                    pieces.push(Piece::Quoted(self.assignment_tildes(s, i == 0, last)));
                }
                part => self.part_pieces(part, false, &mut pieces)?,
            }
        }
        Ok(join(&pieces))
    }

    /// A word as quoted text with only its expansions done: the body of a
    /// here-document, or an arithmetic expression.
    pub fn expand_quoted(&mut self, word: &Word) -> Result<String, Flow> {
        let mut pieces = Vec::new();
        for part in &word.parts {
            self.part_pieces(part, true, &mut pieces)?;
        }
        Ok(join(&pieces))
    }

    /// The subscript of the array `name`: an associative array's key, its
    /// escapes and quotes taken away as a word's are, or an index for
    /// arithmetic, which keeps a backslash before what double quotes do.
    pub fn expand_subscript(&mut self, name: &str, word: &Word) -> Result<String, Flow> {
        if self.assoc(name) {
            return self.expand_quoted(word);
        }
        let mut out = String::new();
        for part in &word.parts {
            match part {
                WordPart::Escaped('\n') => {}
                WordPart::Escaped(c @ ('$' | '`' | '"' | '\\')) => out.push(*c),
                WordPart::Escaped(c) => {
                    out.push('\\');
                    out.push(*c);
                }
                part => {
                    let mut pieces = Vec::new();
                    self.part_pieces(part, true, &mut pieces)?;
                    out.push_str(&join(&pieces));
                }
            }
        }
        Ok(out)
    }

    /// A word as a pattern: quoted parts match themselves.
    pub fn expand_pattern(&mut self, word: &Word) -> Result<String, Flow> {
        let pieces = self.pieces(word, true)?;
        Ok(one_field(pieces).pattern())
    }

    /// A word as a regular expression for `=~`: quoted parts match
    /// themselves.
    pub fn expand_regex(&mut self, word: &Word) -> Result<String, Flow> {
        let pieces = self.pieces(word, true)?;
        Ok(one_field(pieces).regex())
    }

    fn pieces(&mut self, word: &Word, tilde: bool) -> Result<Vec<Piece>, Flow> {
        let mut out = Vec::new();
        let mut parts: &[WordPart] = &word.parts;
        if tilde
            && let Some(WordPart::Lit(first)) = parts.first()
            && first.starts_with('~')
        {
            let slash = first.find('/');
            if slash.is_some() || parts.len() == 1 {
                let end = slash.unwrap_or(first.len());
                if let Some(home) = self.tilde(&first[1..end]) {
                    out.push(Piece::Quoted(home));
                    if end < first.len() {
                        out.push(Piece::Lit(first[end..].to_string()));
                    }
                    parts = &parts[1..];
                }
            }
        }
        for part in parts {
            self.part_pieces(part, false, &mut out)?;
        }
        Ok(out)
    }

    /// What `~name` stands for: `None` to leave it as it is.
    pub fn tilde(&self, name: &str) -> Option<String> {
        match name {
            "" => self.get("HOME").or_else(|| sys::home_of(None)),
            "+" => self.get("PWD"),
            "-" => self.get("OLDPWD"),
            _ => {
                let (sign, digits) = match name.as_bytes()[0] {
                    b'+' => (1, &name[1..]),
                    b'-' => (-1, &name[1..]),
                    _ => (1, name),
                };
                if let Ok(n) = digits.parse::<usize>() {
                    // `~N`: the directory stack, as `dirs +N` lists it.
                    let mut stack = vec![self.get("PWD").unwrap_or_default()];
                    stack.extend(self.dirstack.iter().rev().cloned());
                    let at = if sign > 0 { Some(n) } else { stack.len().checked_sub(n + 1) };
                    return at.and_then(|at| stack.get(at).cloned());
                }
                sys::home_of(Some(name))
            }
        }
    }

    /// Tildes after `=` and `:` in an assignment's unquoted text.
    fn assignment_tildes(&self, text: &str, first: bool, last: bool) -> String {
        let mut out = String::new();
        for (i, segment) in text.split(':').enumerate() {
            if i > 0 {
                out.push(':');
            }
            let starts = i > 0 || first;
            if starts && let Some(rest) = segment.strip_prefix('~') {
                let end = rest.find('/').unwrap_or(rest.len());
                let ends_here = end < rest.len() || i + 1 < text.split(':').count() || last;
                if ends_here && let Some(home) = self.tilde(&rest[..end]) {
                    out.push_str(&home);
                    out.push_str(&rest[end..]);
                    continue;
                }
            }
            out.push_str(segment);
        }
        out
    }

    fn part_pieces(
        &mut self,
        part: &WordPart,
        quoted: bool,
        out: &mut Vec<Piece>,
    ) -> Result<(), Flow> {
        match part {
            WordPart::Lit(s) => {
                out.push(if quoted { Piece::Quoted(s.clone()) } else { Piece::Lit(s.clone()) });
            }
            WordPart::Escaped(c) => out.push(Piece::Quoted(c.to_string())),
            WordPart::Quoted(s) => out.push(Piece::Quoted(s.clone())),
            WordPart::Double(parts) => {
                if !only_list(parts) {
                    out.push(Piece::Quoted(String::new()));
                }
                for part in parts {
                    self.part_pieces(part, true, out)?;
                }
            }
            WordPart::Param(p) => self.param_pieces(p, quoted, out)?,
            WordPart::Command(program) => {
                let output = self.command_subst(program)?;
                out.push(if quoted { Piece::Quoted(output) } else { Piece::Split(output) });
            }
            WordPart::Arith(word) => {
                let expression = self.expand_quoted(word)?;
                let value = self.arith(&expression)?.to_string();
                out.push(if quoted { Piece::Quoted(value) } else { Piece::Split(value) });
            }
            WordPart::ProcSub(output, program) => {
                let path = self.process_subst(*output, program)?;
                out.push(Piece::Quoted(path));
            }
            WordPart::ArrayAssign(assign) => {
                // Outside a declaration command, `a=(x)` is its text.
                let mut text = assign.name.clone();
                text.push_str(if assign.append { "+=(" } else { "=(" });
                if let crate::ast::AssignValue::Array(elements) = &assign.value {
                    let mut values = Vec::new();
                    for (_, word) in elements {
                        values.push(self.expand_string(word)?);
                    }
                    text.push_str(&values.join(" "));
                }
                text.push(')');
                out.push(Piece::Quoted(text));
            }
        }
        Ok(())
    }

    // Parameters.

    fn param_pieces(
        &mut self,
        p: &ParamExp,
        quoted: bool,
        out: &mut Vec<Piece>,
    ) -> Result<(), Flow> {
        if let ParamOp::Bad(text) = &p.op {
            return Err(self.discard(&format!("${{{text}}}: bad substitution")));
        }
        let value = self.param_value(p)?;
        let colon_missing = |colon: bool, value: &PVal| value.missing(colon);
        // As in bash, the pattern and replacement of an unset parameter
        // are not expanded, so their errors do not come.
        let unset = value.missing(false);
        let value = match &p.op {
            ParamOp::None => {
                self.check_set(p, &value)?;
                value
            }
            ParamOp::Length => {
                if value == PVal::Unset && self.opts.nounset {
                    return Err(self.unbound(p));
                }
                let n = match &value {
                    PVal::List(items, _) => items.len(),
                    PVal::One(s) => s.chars().count(),
                    PVal::Unset => 0,
                };
                PVal::One(n.to_string())
            }
            ParamOp::Default(colon, word) => {
                if colon_missing(*colon, &value) {
                    return self.operand_pieces(word, quoted, out);
                }
                value
            }
            ParamOp::Alternative(colon, word) => {
                if !colon_missing(*colon, &value) {
                    return self.operand_pieces(word, quoted, out);
                }
                PVal::Unset
            }
            ParamOp::Assign(colon, word) => {
                if colon_missing(*colon, &value) {
                    let text = self.operand_string(word)?;
                    let (name, index) = if p.indirect {
                        // `${!ref:=word}` assigns to the variable `ref` names.
                        let direct = ParamExp { indirect: false, op: ParamOp::None, ..p.clone() };
                        let target = match self.param_value(&direct)? {
                            PVal::One(target) => target,
                            PVal::List(items, _) => items.join(" "),
                            PVal::Unset => String::new(),
                        };
                        // As in bash, not to an element.
                        let Some((base, None)) = parse_target(&target) else {
                            return Err(self.discard(&format!("{target}: invalid variable name")));
                        };
                        (base, None)
                    } else {
                        let ParamName::Var(name) = &p.name else {
                            let message = format!("${}: cannot assign in this way", param_label(p));
                            return Err(self.discard(&message));
                        };
                        let index = match &p.index {
                            Some(index) => Some(self.expand_subscript(name, index)?),
                            None => None,
                        };
                        (name.clone(), index)
                    };
                    let target = match index {
                        Some(index) => self.assign_element(&name, &index, text.clone(), false),
                        None => self.assign(&name, text.clone(), false),
                    };
                    if let Err(message) = target {
                        // bash's status for this one is 2.
                        return Err(self.discard_with(&message, 2));
                    }
                    PVal::One(text)
                } else {
                    value
                }
            }
            ParamOp::Error(colon, word) => {
                if colon_missing(*colon, &value) {
                    let message = if word.parts.is_empty() {
                        if *colon {
                            "parameter null or not set".to_string()
                        } else {
                            "parameter not set".to_string()
                        }
                    } else {
                        self.operand_string(word)?
                    };
                    let status = if self.posix() && !self.interactive { 2 } else { 1 };
                    let message = format!("{}: {message}", param_label(p));
                    return Err(self.expansion_fatal(&message, status));
                }
                value
            }
            ParamOp::RemovePrefix(..)
            | ParamOp::RemoveSuffix(..)
            | ParamOp::Replace(..)
            | ParamOp::Case(..)
            | ParamOp::Toggle(..)
                if unset =>
            {
                self.check_set(p, &value)?;
                value
            }
            ParamOp::RemovePrefix(longest, word) | ParamOp::RemoveSuffix(longest, word) => {
                self.check_set(p, &value)?;
                let text = self.expand_pattern(word)?;
                let matcher = Pattern::new(&text, self.pattern_options());
                let prefix = matches!(p.op, ParamOp::RemovePrefix(..));
                value.map(|s| {
                    if prefix {
                        match matcher.match_prefix(&s, *longest) {
                            Some(n) => s.chars().skip(n).collect(),
                            None => s,
                        }
                    } else {
                        match matcher.match_suffix(&s, *longest) {
                            Some(n) => s.chars().take(n).collect(),
                            None => s,
                        }
                    }
                })
            }
            ParamOp::Substring(offset, length) => {
                self.check_set(p, &value)?;
                self.substring(p, value, offset, length.as_ref())?
            }
            ParamOp::Replace(kind, pattern_word, replacement) => {
                self.check_set(p, &value)?;
                let text = self.expand_pattern(pattern_word)?;
                let ampersand = self.shopt("patsub_replacement");
                let replacement = match replacement {
                    Some(word) => replacement_parts(self.pieces(word, true)?, ampersand),
                    None => Vec::new(),
                };
                let matcher = Pattern::new(&text, self.pattern_options());
                value.map(|s| replace(&s, &matcher, *kind, &replacement))
            }
            ParamOp::Case(upper, all, pattern_word) => {
                self.check_set(p, &value)?;
                let matcher = match pattern_word {
                    Some(word) => {
                        let text = self.expand_pattern(word)?;
                        Some(Pattern::new(&text, self.pattern_options()))
                    }
                    None => None,
                };
                value.map(|s| change_case(&s, Some(*upper), *all, matcher.as_ref()))
            }
            ParamOp::Toggle(all, pattern_word) => {
                self.check_set(p, &value)?;
                let matcher = match pattern_word {
                    Some(word) => {
                        let text = self.expand_pattern(word)?;
                        Some(Pattern::new(&text, self.pattern_options()))
                    }
                    None => None,
                };
                value.map(|s| change_case(&s, None, *all, matcher.as_ref()))
            }
            ParamOp::Transform(t) => {
                if *t == 'a' {
                    return self.attribute_letters(p, quoted, out);
                }
                if *t == 'A' {
                    let pieces = self.assignment_form(p, &value);
                    emit(pieces, quoted, &self.ifs_first(), out);
                    return Ok(());
                }
                // Every element of an array: its keys and values, as one
                // string of quoted pairs (`K`) or as words (`k`).
                if matches!(t, 'K' | 'k')
                    && let Some((pairs, assoc)) = self.array_pairs(p)
                {
                    let pieces = if *t == 'K' {
                        let quote_value = crate::builtins::double_quote;
                        let pairs = pairs.iter().map(|(k, v)| {
                            if assoc {
                                format!("{} {} ", crate::builtins::quote_key(k), quote_value(v))
                            } else {
                                format!("{k} {}", quote_value(v))
                            }
                        });
                        // bash ends an associative array's pairs with a blank.
                        let text = if assoc {
                            pairs.collect::<String>()
                        } else {
                            pairs.collect::<Vec<_>>().join(" ")
                        };
                        PVal::One(text)
                    } else {
                        let star = p.index.as_ref().and_then(Word::as_literal) == Some("*");
                        PVal::List(pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(), star)
                    };
                    emit(pieces, quoted, &self.ifs_first(), out);
                    return Ok(());
                }
                self.check_set(p, &value)?;
                let result = value.map(|s| match t {
                    // Anything but a whole array is quoted as by `Q`.
                    'Q' | 'K' | 'k' => quote_always(&s),
                    'E' => crate::prompt::decode_escapes(&s),
                    'U' => s.to_uppercase(),
                    'L' => s.to_lowercase(),
                    'u' => {
                        let mut chars = s.chars();
                        match chars.next() {
                            Some(c) => c.to_uppercase().chain(chars).collect(),
                            None => s,
                        }
                    }
                    // `P`, expanded below.
                    _ => s,
                });
                if *t == 'P' { result.map(|s| crate::prompt::expand(self, &s)) } else { result }
            }
            // An error that ends the shell, but only when there is
            // something to transform, as in bash.
            ParamOp::BadTransform(text) => {
                self.check_set(p, &value)?;
                let empty = match &value {
                    PVal::Unset => true,
                    PVal::List(items, _) => items.is_empty(),
                    PVal::One(_) => false,
                };
                if !empty {
                    let message = format!("${{{}@{text}}}: bad substitution", param_label(p));
                    return Err(self.expansion_fatal(&message, 1));
                }
                value
            }
            ParamOp::Names(_) | ParamOp::Keys(_) => value,
            ParamOp::Bad(_) => unreachable!("an error above"),
        };
        emit(value, quoted, &self.ifs_first(), out);
        Ok(())
    }

    /// The pieces of the word of `${x:-word}` and the like, quoted as the
    /// expansion is. Unquoted, the word is part of the expansion's result,
    /// so its text as written is split too, as in bash and dash:
    /// `${opt:+-f }file` gives `-f` and `file`.
    fn operand_pieces(
        &mut self,
        word: &Word,
        quoted: bool,
        out: &mut Vec<Piece>,
    ) -> Result<(), Flow> {
        let start = out.len();
        if !quoted
            && let Some(WordPart::Lit(first)) = word.parts.first()
            && first.starts_with('~')
        {
            let pieces = self.pieces(word, true)?;
            out.extend(pieces);
        } else {
            for part in &word.parts {
                self.part_pieces(part, quoted, out)?;
            }
        }
        if !quoted {
            for piece in &mut out[start..] {
                if let Piece::Lit(text) = piece {
                    *piece = Piece::Split(std::mem::take(text));
                }
            }
        }
        Ok(())
    }

    fn operand_string(&mut self, word: &Word) -> Result<String, Flow> {
        let pieces = self.pieces(word, true)?;
        Ok(join(&pieces))
    }

    /// `set -u`: an unset parameter is an error.
    fn check_set(&mut self, p: &ParamExp, value: &PVal) -> Result<(), Flow> {
        if *value == PVal::Unset
            && self.opts.nounset
            && !matches!(p.name, ParamName::Special('@' | '*'))
        {
            return Err(self.unbound(p));
        }
        Ok(())
    }

    pub fn pattern_options(&self) -> Options {
        Options {
            extglob: self.shopt("extglob"),
            nocase: self.shopt("nocasematch"),
            file_name: false,
            dotglob: false,
        }
    }

    /// The first character of `IFS`, which joins `"$*"`: a space when
    /// `IFS` is unset.
    fn ifs_first(&self) -> String {
        match self.get("IFS") {
            Some(ifs) => ifs.chars().next().map_or_default(String::from),
            None => " ".into(),
        }
    }

    /// What a parameter holds, before its operator.
    fn param_value(&mut self, p: &ParamExp) -> Result<PVal, Flow> {
        if let ParamOp::Names(_) = p.op {
            let ParamName::Var(prefix) = &p.name else { return Ok(PVal::List(Vec::new(), false)) };
            let mut names: Vec<String> = self
                .vars
                .all()
                .into_iter()
                .filter(|(name, var)| name.starts_with(prefix.as_str()) && var.value.is_some())
                .map(|(name, _)| name.to_string())
                .collect();
            names.sort();
            let star = matches!(p.op, ParamOp::Names(false));
            return Ok(PVal::List(names, star));
        }
        if let ParamOp::Keys(all) = p.op {
            let ParamName::Var(name) = &p.name else { return Ok(PVal::List(Vec::new(), !all)) };
            let keys = self.get_value(name).map_or_default(|v| v.keys());
            return Ok(PVal::List(keys, !all));
        }
        if p.indirect && !matches!(p.name, ParamName::Var(_)) {
            // `${!1}` and `${!#}`: the parameter that the value names.
            let direct = ParamExp { indirect: false, op: ParamOp::None, ..p.clone() };
            let target = match self.param_value(&direct)? {
                PVal::One(target) => target,
                PVal::List(items, _) => items.join(" "),
                PVal::Unset => return Ok(PVal::Unset),
            };
            let name = if let Some((base, sub)) = parse_target(&target) {
                let sub = match sub {
                    Some(sub) => Some(self.expand_index(&sub)?),
                    None => None,
                };
                ParamExp {
                    name: ParamName::Var(base),
                    index: sub.map(|sub| Word { parts: vec![WordPart::Lit(sub)] }),
                    op: p.op.clone(),
                    indirect: false,
                    braced: true,
                }
            } else if let Ok(n) = target.parse::<usize>()
                && n > 0
            {
                ParamExp {
                    name: ParamName::Positional(n),
                    index: None,
                    op: p.op.clone(),
                    indirect: false,
                    braced: true,
                }
            } else if target.len() == 1 && "@*#?-$!0".contains(target.as_str()) {
                let c = target.chars().next().unwrap_or('0');
                ParamExp {
                    name: ParamName::Special(c),
                    index: None,
                    op: p.op.clone(),
                    indirect: false,
                    braced: true,
                }
            } else {
                return Err(self.discard(&format!("{target}: invalid variable name")));
            };
            return self.param_value(&name);
        }
        let mut name = match &p.name {
            ParamName::Var(name) => name.clone(),
            ParamName::Positional(n) => {
                return Ok(match self.args.get(n - 1) {
                    Some(arg) => PVal::One(arg.clone()),
                    None => PVal::Unset,
                });
            }
            ParamName::Special(c) => return Ok(self.special(*c, p)),
        };
        let mut index = p.index.clone();
        if p.indirect {
            // `${!name}`: the value of the parameter that `name` names.
            let target = match &index {
                Some(word) => {
                    let lit = word.as_literal().unwrap_or("");
                    if lit == "@" || lit == "*" {
                        let v = self.get_value(&name).map_or_default(|v| v.elements());
                        v.join(" ")
                    } else {
                        let at = self.expand_subscript(&name, word)?;
                        match self.element(&name, &at, None)? {
                            Some(target) => target,
                            None => return Ok(PVal::Unset),
                        }
                    }
                }
                None => match self.get(&self.resolve(&name)) {
                    Some(target) if self.vars.get(&name).is_some_and(|v| v.has(attr::NAMEREF)) => {
                        // `${!ref}` of a reference is the name it refers to.
                        return Ok(PVal::One(
                            self.vars
                                .get(&name)
                                .and_then(|v| v.value.as_ref())
                                .and_then(Value::scalar)
                                .unwrap_or(&target)
                                .to_string(),
                        ));
                    }
                    Some(target) => target,
                    // bash 5.2's error, which comes before `set -u`'s.
                    None => {
                        return Err(self.discard(&format!("{name}: invalid indirect expansion")));
                    }
                },
            };
            index = None;
            match parse_target(&target) {
                Some((base, Some(sub))) => {
                    name = base;
                    index = Some(Word { parts: vec![WordPart::Lit(self.expand_index(&sub)?)] });
                }
                Some((base, None)) => name = base,
                None => {
                    if let Some(c) = target.chars().next()
                        && target.len() == 1
                        && "@*#?-$!0".contains(c)
                    {
                        return Ok(self.special(c, p));
                    }
                    if let Ok(n) = target.parse::<usize>() {
                        return Ok(match self.args.get(n.wrapping_sub(1)) {
                            Some(arg) => PVal::One(arg.clone()),
                            None => PVal::Unset,
                        });
                    }
                    return Err(self.discard(&format!("{target}: invalid variable name")));
                }
            }
        }
        match &index {
            Some(word) => {
                let lit = word.as_literal().unwrap_or("");
                if lit == "@" || lit == "*" {
                    let items = match self.get_value(&name) {
                        Some(value) => value.elements(),
                        None => {
                            return Ok(if matches!(p.op, ParamOp::None) && !self.opts.nounset {
                                PVal::List(Vec::new(), lit == "*")
                            } else {
                                PVal::Unset
                            });
                        }
                    };
                    return Ok(PVal::List(items, lit == "*"));
                }
                let at = self.expand_subscript(&name, word)?;
                // The subscript as written, for an error of `${#`.
                let measured = matches!(p.op, ParamOp::Length)
                    .then(|| if lit.is_empty() { at.clone() } else { lit.to_string() });
                Ok(match self.element(&name, &at, measured.as_deref())? {
                    Some(value) => PVal::One(value),
                    None => PVal::Unset,
                })
            }
            None => Ok(match name.as_str() {
                "RANDOM" if self.vars.get("RANDOM").is_none() => {
                    PVal::One(self.next_random().to_string())
                }
                "SRANDOM" => PVal::One(sys::random_u32().to_string()),
                _ => match self.get(&name) {
                    Some(value) => PVal::One(value),
                    None => PVal::Unset,
                },
            }),
        }
    }

    /// `name[at]`, where `at` is already expanded. As in bash, a negative
    /// index before the start is warned of and reads as unset, but under
    /// `${#`, whose subscript as written is `measured`, it fails the
    /// command, if the variable is set.
    fn element(
        &mut self,
        name: &str,
        at: &str,
        measured: Option<&str>,
    ) -> Result<Option<String>, Flow> {
        let name = self.resolve(name);
        let value = self.get_value(&name);
        match value {
            Some(Value::Assoc(map)) => Ok(map.get(at).cloned()),
            None if self.vars.get(&name).is_some_and(|v| v.has(attr::ASSOC)) => Ok(None),
            value => {
                let index = self.arith(at)?;
                let Some(index) = vars::position(value.as_ref(), index) else {
                    match measured {
                        Some(_) if value.is_none() => {}
                        Some(text) => {
                            return Err(self.discard(&format!("{text}]: bad array subscript")));
                        }
                        None => self.error(&format!("{name}: bad array subscript")),
                    }
                    return Ok(None);
                };
                Ok(match value {
                    Some(Value::Str(s)) => (index == 0).then_some(s),
                    Some(Value::Array(map)) => map.get(&index).cloned(),
                    _ => None,
                })
            }
        }
    }

    fn special(&mut self, c: char, p: &ParamExp) -> PVal {
        match c {
            '@' | '*' => {
                if let ParamOp::Substring(..) = p.op {
                    // `${@:0}` starts at `$0`.
                    let mut all = vec![self.arg0.clone()];
                    all.extend(self.args.iter().cloned());
                    return PVal::List(all, c == '*');
                }
                PVal::List(self.args.clone(), c == '*')
            }
            '#' => PVal::One(self.args.len().to_string()),
            '?' => PVal::One(self.status.to_string()),
            '-' => PVal::One(self.flags()),
            '$' => PVal::One(self.pid.to_string()),
            '!' => match self.last_bg {
                Some(pid) => PVal::One(pid.to_string()),
                None => PVal::Unset,
            },
            '0' => PVal::One(self.arg0.clone()),
            _ => PVal::Unset,
        }
    }

    /// The error of `set -u` for an unset parameter, which ends a script:
    /// with status 2 in POSIX mode, as in dash, and 1 as in bash.
    fn unbound(&mut self, p: &ParamExp) -> Flow {
        let status = if self.posix() { 2 } else { 1 };
        self.expansion_fatal(&format!("{}: unbound variable", param_label(p)), status)
    }

    fn substring(
        &mut self,
        p: &ParamExp,
        value: PVal,
        offset: &Word,
        length: Option<&Word>,
    ) -> Result<PVal, Flow> {
        // Nothing to take a part of: the offset and length are not
        // evaluated, as in bash.
        if matches!(&value, PVal::Unset)
            || matches!(&value, PVal::List(items, _) if items.is_empty())
        {
            return Ok(value);
        }
        let offset_text = self.expand_quoted(offset)?;
        let offset = self.arith(&offset_text)?;
        let length = match length {
            Some(word) => {
                let text = self.expand_quoted(word)?;
                Some(self.arith(&text)?)
            }
            None => None,
        };
        let slice = |count: usize,
                     offset: i64,
                     length: Option<i64>|
         -> Result<Option<(usize, usize)>, String> {
            let count = count as i64;
            let start = if offset < 0 { count + offset } else { offset };
            if start < 0 || start > count {
                return Ok(None);
            }
            let end = match length {
                None => count,
                Some(n) if n < 0 => {
                    let end = count + n;
                    if end < start {
                        return Err(format!("{n}: substring expression < 0"));
                    }
                    end
                }
                Some(n) => (start + n).min(count),
            };
            Ok(Some((start as usize, end as usize)))
        };
        Ok(match value {
            PVal::Unset => PVal::Unset,
            PVal::One(s) => {
                let chars: Vec<char> = s.chars().collect();
                match slice(chars.len(), offset, length) {
                    Ok(Some((a, b))) => PVal::One(chars[a..b].iter().collect()),
                    Ok(None) => PVal::One(String::new()),
                    Err(message) => return Err(self.discard(&message)),
                }
            }
            PVal::List(items, star) => {
                if let Some(n) = length
                    && n < 0
                {
                    return Err(self.discard(&format!("{n}: substring expression < 0")));
                }
                // Arrays are sliced by index; `$@` by position.
                let positions: Vec<(i64, String)> = match (&p.name, self.get_value_of(p)) {
                    (ParamName::Var(_), Some(Value::Array(map))) => map.into_iter().collect(),
                    _ => items.into_iter().enumerate().map(|(i, v)| (i as i64, v)).collect(),
                };
                let top = positions.last().map_or(0, |(i, _)| i + 1);
                let start = if offset < 0 { top + offset } else { offset };
                if start < 0 {
                    return Ok(PVal::List(Vec::new(), star));
                }
                let taken: Vec<String> = positions
                    .into_iter()
                    .filter(|(i, _)| *i >= start)
                    .map(|(_, v)| v)
                    .take(length.map_or(usize::MAX, |n| n as usize))
                    .collect();
                PVal::List(taken, star)
            }
        })
    }

    fn get_value_of(&self, p: &ParamExp) -> Option<Value> {
        match &p.name {
            ParamName::Var(name) => self.get_value(name),
            _ => None,
        }
    }

    /// The keys and values of an array named with `[@]` or `[*]`, and
    /// whether it is associative.
    fn array_pairs(&self, p: &ParamExp) -> Option<(Vec<(String, String)>, bool)> {
        let ParamName::Var(name) = &p.name else { return None };
        let all = p.index.as_ref().and_then(Word::as_literal).is_some_and(|i| i == "@" || i == "*");
        match self.get_value(name)? {
            whole @ (Value::Array(_) | Value::Assoc(_)) if all => {
                let assoc = matches!(whole, Value::Assoc(_));
                Some((whole.keys().into_iter().zip(whole.elements()).collect(), assoc))
            }
            _ => None,
        }
    }

    /// `${name@A}`, as bash 5.2 gives it: for every element of an array
    /// the words of its `declare` (`declare`, its options, the
    /// assignment); otherwise an assignment of the value expanded, with
    /// `declare` and the options when the variable has attributes; for
    /// `$@` and `$*` the `set --` that makes them.
    fn assignment_form(&self, p: &ParamExp, value: &PVal) -> PVal {
        let star = p.index.as_ref().and_then(Word::as_literal) == Some("*");
        let name = match &p.name {
            ParamName::Special(c @ ('@' | '*')) => {
                if self.args.is_empty() {
                    return PVal::Unset;
                }
                let mut words = vec!["set".to_string(), "--".to_string()];
                words.extend(self.args.iter().map(|arg| quote_always(arg)));
                return PVal::List(words, *c == '*');
            }
            ParamName::Var(name) => self.resolve(name),
            _ => return PVal::Unset,
        };
        let Some(var) = self.vars.get(&name) else { return PVal::Unset };
        let mut attrs = var.attrs;
        match var.value {
            Some(Value::Array(_)) => attrs |= attr::ARRAY,
            Some(Value::Assoc(_)) => attrs |= attr::ASSOC,
            _ => {}
        }
        let flags = attribute_flags(attrs);
        let all = p.index.as_ref().and_then(Word::as_literal).is_some_and(|i| i == "@" || i == "*");
        if all && attrs & (attr::ARRAY | attr::ASSOC) != 0 {
            let line = crate::builtins::declare_line(self, &name).unwrap_or_default();
            let words: Vec<String> = line.splitn(3, ' ').map(str::to_string).collect();
            return PVal::List(words, star);
        }
        let head = if flags.is_empty() { String::new() } else { format!("declare -{flags} ") };
        let value = match value {
            PVal::One(s) => Some(s),
            PVal::List(items, _) => items.first(),
            PVal::Unset => None,
        };
        match value {
            Some(s) => PVal::One(format!("{head}{name}={}", quote_always(s))),
            None if !flags.is_empty() => PVal::One(format!("{head}{name}")),
            None => PVal::Unset,
        }
    }

    fn attribute_letters(
        &mut self,
        p: &ParamExp,
        quoted: bool,
        out: &mut Vec<Piece>,
    ) -> Result<(), Flow> {
        let attrs = match &p.name {
            ParamName::Var(name) => self.vars.get(name).map_or(0, |v| {
                let mut a = v.attrs;
                if matches!(v.value, Some(Value::Array(_))) {
                    a |= attr::ARRAY;
                }
                if matches!(v.value, Some(Value::Assoc(_))) {
                    a |= attr::ASSOC;
                }
                a
            }),
            _ => 0,
        };
        emit(PVal::One(attribute_flags(attrs)), quoted, &self.ifs_first(), out);
        Ok(())
    }

    // Field splitting.

    /// Splits pieces into fields at `IFS`.
    fn split(&self, pieces: Vec<Piece>) -> Vec<Field> {
        let ifs = self.get("IFS").unwrap_or_else(|| " \t\n".into());
        let is_ifs_space = |c: char| (c == ' ' || c == '\t' || c == '\n') && ifs.contains(c);
        let mut fields = Vec::new();
        let mut current = Field::default();
        let mut live = false;
        // After a field ended at IFS white space, the non-space delimiter
        // that may follow belongs to the same delimiter.
        let mut ended_by_space = false;
        for piece in pieces {
            match piece {
                Piece::Lit(s) => {
                    current.push(&s, false);
                    live = true;
                    ended_by_space = false;
                }
                Piece::Quoted(s) => {
                    current.push(&s, true);
                    live = true;
                    ended_by_space = false;
                }
                Piece::Break => {
                    fields.push(std::mem::take(&mut current));
                    live = false;
                    ended_by_space = false;
                }
                Piece::SoftBreak(_) => {
                    if live {
                        fields.push(std::mem::take(&mut current));
                        live = false;
                    }
                    ended_by_space = false;
                }
                Piece::Split(s) => {
                    if ifs.is_empty() {
                        if !s.is_empty() {
                            current.push(&s, false);
                            live = true;
                        }
                        continue;
                    }
                    let mut text = String::new();
                    for c in s.chars() {
                        if !ifs.contains(c) {
                            text.push(c);
                            live = true;
                            ended_by_space = false;
                            continue;
                        }
                        if !text.is_empty() {
                            current.push(&std::mem::take(&mut text), false);
                        }
                        if is_ifs_space(c) {
                            if live {
                                fields.push(std::mem::take(&mut current));
                                live = false;
                                ended_by_space = true;
                            }
                        } else if live {
                            fields.push(std::mem::take(&mut current));
                            live = false;
                            ended_by_space = false;
                        } else if ended_by_space {
                            ended_by_space = false;
                        } else {
                            // Two delimiters with nothing between: an empty
                            // field.
                            fields.push(Field::default());
                        }
                    }
                    if !text.is_empty() {
                        current.push(&text, false);
                    }
                }
            }
        }
        if live {
            fields.push(current);
        }
        fields
    }
}

/// Puts a parameter's value into pieces, quoted or not.
fn emit(value: PVal, quoted: bool, ifs_first: &str, out: &mut Vec<Piece>) {
    match value {
        PVal::Unset => {}
        PVal::One(s) => out.push(if quoted { Piece::Quoted(s) } else { Piece::Split(s) }),
        PVal::List(items, star) => {
            if quoted && star {
                out.push(Piece::Quoted(items.join(ifs_first)));
            } else {
                for (i, item) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push(if quoted {
                            Piece::Break
                        } else {
                            Piece::SoftBreak(if star { ifs_first.to_string() } else { " ".into() })
                        });
                    }
                    out.push(if quoted { Piece::Quoted(item) } else { Piece::Split(item) });
                }
            }
        }
    }
}

/// Pieces as one string: the elements of `"$@"` and `$@` joined by
/// spaces, and those of `$*` by the first character of `IFS`.
fn join(pieces: &[Piece]) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Lit(s) | Piece::Quoted(s) | Piece::Split(s) | Piece::SoftBreak(s) => {
                out.push_str(s);
            }
            Piece::Break => out.push(' '),
        }
    }
    out
}

/// Pieces as one field, without splitting.
fn one_field(pieces: Vec<Piece>) -> Field {
    let mut field = Field::default();
    for piece in pieces {
        match piece {
            Piece::Lit(s) | Piece::Split(s) => field.push(&s, false),
            Piece::Quoted(s) => field.push(&s, true),
            Piece::Break => field.push(" ", true),
            Piece::SoftBreak(s) => field.push(&s, true),
        }
    }
    field
}

/// The name of a parameter, for messages.
fn param_label(p: &ParamExp) -> String {
    let label = match &p.name {
        ParamName::Var(name) => match &p.index {
            Some(index) => format!("{name}[{}]", index.as_literal().unwrap_or("...")),
            None => name.clone(),
        },
        ParamName::Positional(n) => n.to_string(),
        ParamName::Special(c) => c.to_string(),
    };
    if p.indirect { format!("!{label}") } else { label }
}

/// `name` or `name[sub]`, as `${!ref}` and `declare -n` hold them.
fn parse_target(target: &str) -> Option<(String, Option<String>)> {
    match target.find('[') {
        Some(open) if target.ends_with(']') => {
            let base = &target[..open];
            crate::parse::is_name(base)
                .then(|| (base.to_string(), Some(target[open + 1..target.len() - 1].to_string())))
        }
        _ => crate::parse::is_name(target).then(|| (target.to_string(), None)),
    }
}

/// A part of the replacement of `${x/pattern/string}`.
enum Replacement {
    Text(String),
    /// An unquoted `&`, with `patsub_replacement`: what matched.
    Matched,
}

/// The replacement's parts, from its expanded pieces, as in bash: in the
/// text as written, a backslash takes the next character as it is. With
/// `patsub_replacement`, an unquoted `&` stands for what matched, and in
/// an unquoted expansion's value `\&` and `\\` stand for `&` and `\`.
fn replacement_parts(pieces: Vec<Piece>, ampersand: bool) -> Vec<Replacement> {
    let mut parts = Vec::new();
    let mut text = String::new();
    for piece in pieces {
        let (written, s) = match piece {
            Piece::Lit(s) => (true, s),
            Piece::Split(s) => (false, s),
            Piece::Quoted(s) | Piece::SoftBreak(s) => {
                text.push_str(&s);
                continue;
            }
            Piece::Break => {
                text.push(' ');
                continue;
            }
        };
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\\' => match it.peek().copied() {
                    Some(n) if written || ampersand && (n == '&' || n == '\\') => {
                        text.push(n);
                        it.next();
                    }
                    _ => text.push('\\'),
                },
                '&' if ampersand => {
                    parts.push(Replacement::Text(std::mem::take(&mut text)));
                    parts.push(Replacement::Matched);
                }
                c => text.push(c),
            }
        }
    }
    parts.push(Replacement::Text(text));
    parts
}

/// `${s/pattern/replacement}` and its kin.
fn replace(s: &str, matcher: &Pattern, kind: ReplaceKind, replacement: &[Replacement]) -> String {
    let chars: Vec<char> = s.chars().collect();
    let render = |matched: &[char]| -> String {
        let mut out = String::new();
        for part in replacement {
            match part {
                Replacement::Text(text) => out.push_str(text),
                Replacement::Matched => out.extend(matched),
            }
        }
        out
    };
    match kind {
        ReplaceKind::Prefix => {
            let text: String = chars.iter().collect();
            match matcher.match_prefix(&text, true) {
                Some(n) => {
                    let mut out = render(&chars[..n]);
                    out.extend(&chars[n..]);
                    out
                }
                None => s.to_string(),
            }
        }
        ReplaceKind::Suffix => {
            let text: String = chars.iter().collect();
            match matcher.match_suffix(&text, true) {
                Some(n) => {
                    let mut out: String = chars[..n].iter().collect();
                    out.push_str(&render(&chars[n..]));
                    out
                }
                None => s.to_string(),
            }
        }
        ReplaceKind::First | ReplaceKind::All => {
            // As in bash: an empty pattern replaces nothing, and an empty
            // string only if the pattern matches it. Otherwise an empty
            // match counts too, and the character after it is kept before
            // the next look, which starts before the end.
            if matcher.is_empty() {
                return s.to_string();
            }
            if chars.is_empty() {
                return if matcher.matches("") { render(&[]) } else { String::new() };
            }
            let mut out = String::new();
            let mut at = 0;
            while at < chars.len() {
                let Some((start, end)) = matcher.find(&chars, at, true) else { break };
                out.extend(&chars[at..start]);
                out.push_str(&render(&chars[start..end]));
                at = end;
                if start == end && at < chars.len() {
                    out.push(chars[at]);
                    at += 1;
                }
                if kind == ReplaceKind::First {
                    break;
                }
            }
            out.extend(&chars[at.min(chars.len())..]);
            out
        }
    }
}

/// `${s^}` and its kin: to upper case (`Some(true)`), to lower case, or
/// swapped (`None`), the first letter or `all`.
fn change_case(s: &str, upper: Option<bool>, all: bool, matcher: Option<&Pattern>) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        let selected = (all || i == 0) && matcher.is_none_or(|m| m.matches(&c.to_string()));
        let to_upper = upper.unwrap_or(c.is_lowercase());
        if !selected {
            out.push(c);
        } else if to_upper {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The letters of `declare`'s options for a set of attributes.
pub fn attribute_flags(attrs: u16) -> String {
    let mut out = String::new();
    for (bit, letter) in [
        (attr::ARRAY, 'a'),
        (attr::ASSOC, 'A'),
        (attr::INTEGER, 'i'),
        (attr::LOWER, 'l'),
        (attr::NAMEREF, 'n'),
        (attr::READONLY, 'r'),
        (attr::TRACE, 't'),
        (attr::UPPER, 'u'),
        (attr::EXPORT, 'x'),
    ] {
        if attrs & bit != 0 {
            out.push(letter);
        }
    }
    out
}

/// Whether `s` can be written as it is, needing no quotes.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_./=:,+@%^-".contains(c))
}

/// `s` quoted for reuse as shell input, as `${s@Q}` and `printf %q` do:
/// in single quotes, or as `$'...'` if it holds control characters.
pub fn quote(s: &str) -> String {
    if plain(s) {
        return s.to_string();
    }
    quote_always(s)
}

/// `s` as bash names a command or a directory in some messages: as it
/// is, or as `$'...'` if it holds characters that do not print.
pub fn printable(s: &str) -> String {
    if s.chars().any(|c| c.is_control() || text::char_byte(c).is_some()) {
        quote_always(s)
    } else {
        s.to_string()
    }
}

/// Whether `s` holds a character the shell would read as more than
/// itself, as bash's sh_contains_shell_metas has it: `~` only where a
/// tilde expands, `#` only first.
pub fn has_shell_metas(s: &str) -> bool {
    let mut previous = None;
    for (i, c) in s.chars().enumerate() {
        match c {
            ' ' | '\t' | '\n' | '\'' | '"' | '\\' | '|' | '&' | ';' | '(' | ')' | '<' | '>'
            | '!' | '{' | '}' | '*' | '[' | '?' | ']' | '^' | '$' | '`' => return true,
            '~' if matches!(previous, None | Some('=' | ':')) => return true,
            '#' if i == 0 => return true,
            _ => {}
        }
        previous = Some(c);
    }
    false
}

/// `s` in quotes even when it needs none.
pub fn quote_always(s: &str) -> String {
    let control = s.chars().any(|c| c.is_control() || text::char_byte(c).is_some());
    if !control {
        return format!("'{}'", s.replace('\'', "'\\''"));
    }
    let mut out = String::from("$'");
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\E"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0b' => out.push_str("\\v"),
            '\x0c' => out.push_str("\\f"),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c if text::char_byte(c).is_some() => {
                out.push_str(&format!("\\{:03o}", text::char_byte(c).unwrap_or(0)));
            }
            c if c.is_control() => out.push_str(&format!("\\{:03o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
