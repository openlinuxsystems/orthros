// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shell patterns (XCU 2.14): `*`, `?`, bracket expressions with ranges
//! and character classes, and with `shopt -s extglob` bash's `?(...)`,
//! `*(...)`, `+(...)`, `@(...)` and `!(...)`.
//!
//! A pattern comes as text in which a backslash quotes the character after
//! it: the expander writes quoted parts of a word that way, so `"*"` in a
//! `case` matches only a star. Patterns are used for `case`, `[[ == ]]`,
//! the `${name#pattern}` family and, through [`crate::glob`], file names.

/// How a pattern matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub extglob: bool,
    /// `nocasematch` and `nocaseglob`: letters match either case.
    pub nocase: bool,
    /// A file name: a leading `.` must be matched by a `.` in the pattern
    /// (unless `dotglob`), and nothing matches `/`.
    pub file_name: bool,
    pub dotglob: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Char(char),
    Any,
    Star,
    Class {
        negate: bool,
        items: Vec<Item>,
    },
    /// An extended pattern: its kind (`?`, `*`, `+`, `@` or `!`) and its
    /// alternatives.
    Ext(char, Vec<Vec<Tok>>),
}

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Char(char),
    Range(char, char),
    Named(String),
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Pattern {
    toks: Vec<Tok>,
    options: Options,
    /// The text it matches, when it has nothing special in it.
    literal: Option<String>,
}

/// Whether `pattern` has an unquoted `*`, `?` or `[`, or an extended
/// pattern: whether it can match more than its own text.
pub fn has_meta(pattern: &str, extglob: bool) -> bool {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '*' | '?' => return true,
            '[' if closing_bracket(&chars, i).is_some() => return true,
            '+' | '@' | '!' if extglob && chars.get(i + 1) == Some(&'(') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// The pattern's text with its quoting backslashes taken out.
pub fn unescape(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `text` with the characters patterns treat specially quoted, so that it
/// matches only itself.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '(' | ')' | '|' | '!' | '@' | '+' | '^') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Where the `]` that closes the bracket expression opening at `open` is.
fn closing_bracket(chars: &[char], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if matches!(chars.get(i), Some('!' | '^')) {
        i += 1;
    }
    // A `]` first is part of the set.
    if chars.get(i) == Some(&']') {
        i += 1;
    }
    while i < chars.len() {
        match chars[i] {
            ']' => return Some(i),
            '\\' => i += 1,
            '[' if matches!(chars.get(i + 1), Some(':' | '=' | '.')) => {
                let kind = chars[i + 1];
                let mut j = i + 2;
                while j + 1 < chars.len() && !(chars[j] == kind && chars[j + 1] == ']') {
                    j += 1;
                }
                if j + 1 < chars.len() {
                    i = j + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

impl Pattern {
    pub fn new(pattern: &str, options: Options) -> Pattern {
        let chars: Vec<char> = pattern.chars().collect();
        let mut pos = 0;
        let toks = compile(&chars, &mut pos, options, false);
        let literal = if toks.iter().all(|t| matches!(t, Tok::Char(_))) && !options.nocase {
            Some(toks.iter().map(|t| if let Tok::Char(c) = t { *c } else { ' ' }).collect())
        } else {
            None
        };
        Pattern { toks, options, literal }
    }

    /// Whether the pattern matches all of `text`.
    pub fn matches(&self, text: &str) -> bool {
        if let Some(literal) = &self.literal {
            return text == literal;
        }
        let chars: Vec<char> = text.chars().collect();
        self.matches_chars(&chars)
    }

    fn matches_chars(&self, chars: &[char]) -> bool {
        if self.options.file_name
            && !self.options.dotglob
            && chars.first() == Some(&'.')
            && !matches!(self.toks.first(), Some(Tok::Char('.')))
        {
            return false;
        }
        match_here(&self.toks, chars, self.options)
    }

    /// The length in characters of the shortest (or longest) start of
    /// `text` the pattern matches, for `${name#pattern}`.
    pub fn match_prefix(&self, text: &str, longest: bool) -> Option<usize> {
        let chars: Vec<char> = text.chars().collect();
        let mut lengths: Box<dyn Iterator<Item = usize>> =
            if longest { Box::new((0..=chars.len()).rev()) } else { Box::new(0..=chars.len()) };
        lengths.find(|&n| match_here(&self.toks, &chars[..n], self.options))
    }

    /// Where the shortest (or longest) end of `text` the pattern matches
    /// starts, in characters, for `${name%pattern}`.
    pub fn match_suffix(&self, text: &str, longest: bool) -> Option<usize> {
        let chars: Vec<char> = text.chars().collect();
        let mut starts: Box<dyn Iterator<Item = usize>> =
            if longest { Box::new(0..=chars.len()) } else { Box::new((0..=chars.len()).rev()) };
        starts.find(|&s| match_here(&self.toks, &chars[s..], self.options))
    }

    /// The first match in `chars` at or after `from`: the leftmost start,
    /// and the longest match there, for `${name/pattern/text}`. An empty
    /// match counts only if `empty_ok`.
    pub fn find(&self, chars: &[char], from: usize, empty_ok: bool) -> Option<(usize, usize)> {
        if let Some(literal) = &self.literal {
            if literal.is_empty() {
                return if empty_ok { Some((from, from)) } else { None };
            }
            let needle: Vec<char> = literal.chars().collect();
            return (from..=chars.len().saturating_sub(needle.len()))
                .find(|&s| chars[s..].starts_with(&needle))
                .map(|s| (s, s + needle.len()));
        }
        for start in from..=chars.len() {
            for end in (start..=chars.len()).rev() {
                if end == start && !empty_ok {
                    break;
                }
                if match_here(&self.toks, &chars[start..end], self.options) {
                    return Some((start, end));
                }
            }
        }
        None
    }
}

/// Compiles the pattern from `pos`; inside an extended pattern, stops at
/// the `|` or `)` that ends an alternative.
fn compile(chars: &[char], pos: &mut usize, options: Options, nested: bool) -> Vec<Tok> {
    let mut toks = Vec::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        if nested && (c == '|' || c == ')') {
            break;
        }
        match c {
            '\\' => {
                *pos += 1;
                toks.push(Tok::Char(chars.get(*pos).copied().unwrap_or('\\')));
                *pos += 1;
            }
            '*' | '?' | '+' | '@' | '!' if options.extglob && chars.get(*pos + 1) == Some(&'(') => {
                if let Some(alternatives) = compile_ext(chars, pos, options) {
                    toks.push(Tok::Ext(c, alternatives));
                } else {
                    toks.push(Tok::Char(c));
                    *pos += 1;
                }
            }
            '*' => {
                // Several stars match what one does.
                if toks.last() != Some(&Tok::Star) {
                    toks.push(Tok::Star);
                }
                *pos += 1;
            }
            '?' => {
                toks.push(Tok::Any);
                *pos += 1;
            }
            '[' => match closing_bracket(chars, *pos) {
                Some(close) => {
                    toks.push(class(&chars[*pos + 1..close]));
                    *pos = close + 1;
                }
                None => {
                    toks.push(Tok::Char('['));
                    *pos += 1;
                }
            },
            _ => {
                toks.push(Tok::Char(c));
                *pos += 1;
            }
        }
    }
    toks
}

/// `x(a|b)`, at `pos`: its alternatives, or `None` if it is not closed.
fn compile_ext(chars: &[char], pos: &mut usize, options: Options) -> Option<Vec<Vec<Tok>>> {
    let start = *pos;
    *pos += 2;
    let mut alternatives = Vec::new();
    loop {
        alternatives.push(compile(chars, pos, options, true));
        match chars.get(*pos) {
            Some('|') => *pos += 1,
            Some(')') => {
                *pos += 1;
                return Some(alternatives);
            }
            _ => {
                *pos = start;
                return None;
            }
        }
    }
}

/// The inside of a bracket expression.
fn class(inner: &[char]) -> Tok {
    let mut i = 0;
    let negate = matches!(inner.first(), Some('!' | '^'));
    if negate {
        i = 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < inner.len() {
        let mut c = inner[i];
        if c == '[' && matches!(inner.get(i + 1), Some(':' | '=' | '.')) {
            let kind = inner[i + 1];
            let mut j = i + 2;
            while j + 1 < inner.len() && !(inner[j] == kind && inner[j + 1] == ']') {
                j += 1;
            }
            if j + 1 < inner.len() {
                let name: String = inner[i + 2..j].iter().collect();
                if kind == ':' {
                    items.push(Item::Named(name));
                } else {
                    // `[=c=]` and `[.c.]`: the character itself.
                    for c in name.chars() {
                        items.push(Item::Char(c));
                    }
                }
                i = j + 2;
                first = false;
                continue;
            }
        }
        if c == '\\' && i + 1 < inner.len() {
            i += 1;
            c = inner[i];
        } else if c == ']' && !first {
            break;
        }
        first = false;
        if inner.get(i + 1) == Some(&'-') && i + 2 < inner.len() {
            let mut end = inner[i + 2];
            let mut skip = 3;
            if end == '\\' && i + 3 < inner.len() {
                end = inner[i + 3];
                skip = 4;
            }
            items.push(Item::Range(c, end));
            i += skip;
            continue;
        }
        items.push(Item::Char(c));
        i += 1;
    }
    Tok::Class { negate, items }
}

fn named_class(name: &str, c: char) -> bool {
    match name {
        "alnum" => c.is_alphanumeric(),
        "alpha" => c.is_alphabetic(),
        "blank" => c == ' ' || c == '\t',
        "cntrl" => c.is_control(),
        "digit" => c.is_ascii_digit(),
        "graph" => !c.is_whitespace() && !c.is_control() && c != ' ',
        "lower" => c.is_lowercase(),
        "print" => !c.is_control(),
        "punct" => c.is_ascii_punctuation(),
        "space" => c.is_whitespace(),
        "upper" => c.is_uppercase(),
        "xdigit" => c.is_ascii_hexdigit(),
        "word" => c.is_alphanumeric() || c == '_',
        _ => false,
    }
}

fn class_matches(negate: bool, items: &[Item], c: char, options: Options) -> bool {
    let test = |c: char| {
        items.iter().any(|item| match item {
            Item::Char(x) => *x == c,
            Item::Range(lo, hi) => (*lo..=*hi).contains(&c),
            Item::Named(name) => named_class(name, c),
        })
    };
    let mut found = test(c);
    if !found && options.nocase {
        found = c.to_lowercase().any(test) || c.to_uppercase().any(test);
    }
    found != negate
}

fn same(a: char, b: char, options: Options) -> bool {
    a == b || options.nocase && a.to_lowercase().eq(b.to_lowercase())
}

/// Whether `toks` match all of `text`.
fn match_here(toks: &[Tok], text: &[char], options: Options) -> bool {
    if toks.iter().any(|t| matches!(t, Tok::Ext(..))) {
        return match_ext(toks, text, options);
    }
    // Without extended patterns, the usual backtracking to the last star
    // is enough, and linear in practice.
    let (mut t, mut s) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while s < text.len() {
        let c = text[s];
        let step = match toks.get(t) {
            Some(Tok::Star) if !(options.file_name && c == '/') => {
                star = Some((t, s));
                t += 1;
                continue;
            }
            Some(Tok::Char(x)) => same(*x, c, options),
            Some(Tok::Any) => !(options.file_name && c == '/'),
            Some(Tok::Class { negate, items }) => {
                !(options.file_name && c == '/') && class_matches(*negate, items, c, options)
            }
            _ => false,
        };
        if step {
            t += 1;
            s += 1;
        } else if let Some((st, ss)) = star {
            if options.file_name && text[ss] == '/' {
                return false;
            }
            t = st + 1;
            s = ss + 1;
            star = Some((st, ss + 1));
        } else {
            return false;
        }
    }
    toks[t..].iter().all(|tok| *tok == Tok::Star)
}

/// Matching with extended patterns, by trying each way of splitting the
/// text.
fn match_ext(toks: &[Tok], text: &[char], options: Options) -> bool {
    let Some(first) = toks.first() else { return text.is_empty() };
    let rest = &toks[1..];
    match first {
        Tok::Char(x) => {
            text.first().is_some_and(|c| same(*x, *c, options))
                && match_ext(rest, &text[1..], options)
        }
        Tok::Any => !text.is_empty() && match_ext(rest, &text[1..], options),
        Tok::Class { negate, items } => {
            text.first().is_some_and(|c| class_matches(*negate, items, *c, options))
                && match_ext(rest, &text[1..], options)
        }
        Tok::Star => (0..=text.len()).any(|n| match_ext(rest, &text[n..], options)),
        Tok::Ext(kind, alternatives) => {
            let one = |part: &[char]| alternatives.iter().any(|alt| match_ext(alt, part, options));
            match kind {
                '@' => (0..=text.len())
                    .any(|n| one(&text[..n]) && match_ext(rest, &text[n..], options)),
                '?' => {
                    match_ext(rest, text, options)
                        || (0..=text.len())
                            .any(|n| one(&text[..n]) && match_ext(rest, &text[n..], options))
                }
                '!' => (0..=text.len())
                    .any(|n| !one(&text[..n]) && match_ext(rest, &text[n..], options)),
                // `*` and `+`: one or more matches, each taking at least a
                // character, then the rest; `*` may also match nothing.
                _ => {
                    if *kind == '*' && match_ext(rest, text, options) {
                        return true;
                    }
                    repeat(alternatives, rest, text, options)
                }
            }
        }
    }
}

/// One or more matches of `alternatives`, then `rest`.
fn repeat(alternatives: &[Vec<Tok>], rest: &[Tok], text: &[char], options: Options) -> bool {
    (1..=text.len()).any(|n| {
        alternatives.iter().any(|alt| match_ext(alt, &text[..n], options))
            && (match_ext(rest, &text[n..], options)
                || repeat(alternatives, rest, &text[n..], options))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> bool {
        Pattern::new(pattern, Options::default()).matches(text)
    }

    fn x(pattern: &str, text: &str) -> bool {
        Pattern::new(pattern, Options { extglob: true, ..Options::default() }).matches(text)
    }

    #[test]
    fn matches_the_basics() {
        assert!(m("*", ""));
        assert!(m("a*c", "abbbc"));
        assert!(!m("a*c", "abbbd"));
        assert!(m("?b?", "abc"));
        assert!(m("*.txt", "notes.txt"));
        assert!(m("a**b", "ab"));
        assert!(!m("abc", "ab"));
        assert!(m("*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaab"));
        assert!(!m("*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn matches_bracket_expressions() {
        assert!(m("[abc]", "b"));
        assert!(!m("[!abc]", "b"));
        assert!(m("[^abc]", "d"));
        assert!(m("[a-z]x", "qx"));
        assert!(m("[]a]", "]"));
        assert!(m("[!]a]", "b"));
        assert!(m("[[:digit:]][[:alpha:]]", "1a"));
        assert!(!m("[[:upper:]]", "a"));
        assert!(m("[a-]", "-"));
        assert!(m("[", "["));
        assert!(m("a[", "a["));
        assert!(m("[\\]]", "]"));
    }

    #[test]
    fn honours_quoting() {
        assert!(m("\\*", "*"));
        assert!(!m("\\*", "a"));
        assert!(m("a\\?c", "a?c"));
        assert!(!has_meta("a\\*b", false));
        assert!(has_meta("a*b", false));
        assert!(has_meta("[ab]", false));
        assert!(!has_meta("[ab", false));
        assert_eq!(unescape("a\\*b\\\\"), "a*b\\");
        assert!(m(&escape("a*[b]?"), "a*[b]?"));
    }

    #[test]
    fn matches_extended_patterns() {
        assert!(x("@(foo|bar)", "bar"));
        assert!(!x("@(foo|bar)", "baz"));
        assert!(x("?(a)b", "b"));
        assert!(x("?(a)b", "ab"));
        assert!(x("*(ab)", ""));
        assert!(x("*(ab)", "ababab"));
        assert!(!x("+(ab)", ""));
        assert!(x("+(ab|c)d", "abccabd"));
        assert!(x("!(*.c)", "main.h"));
        assert!(!x("!(*.c)", "main.c"));
        assert!(x("*.!(c|h)", "x.o"));
        assert!(!x("*.!(c|h)", "x.c"));
    }

    #[test]
    fn treats_file_names_specially() {
        let files = Options { file_name: true, ..Options::default() };
        assert!(!Pattern::new("*", files).matches(".hidden"));
        assert!(Pattern::new(".*", files).matches(".hidden"));
        assert!(!Pattern::new("a*b", files).matches("a/b"));
        let dot = Options { dotglob: true, ..files };
        assert!(Pattern::new("*", dot).matches(".hidden"));
    }

    #[test]
    fn ignores_case_when_asked() {
        let nocase = Options { nocase: true, ..Options::default() };
        assert!(Pattern::new("hello*", nocase).matches("HeLLo world"));
        assert!(Pattern::new("[a-c]", nocase).matches("B"));
    }

    #[test]
    fn finds_prefixes_suffixes_and_matches() {
        let star_slash = Pattern::new("*/", Options::default());
        assert_eq!(star_slash.match_prefix("a/b/c", false), Some(2));
        assert_eq!(star_slash.match_prefix("a/b/c", true), Some(4));
        let dot_star = Pattern::new(".*", Options::default());
        assert_eq!(dot_star.match_suffix("x.tar.gz", false), Some(5));
        assert_eq!(dot_star.match_suffix("x.tar.gz", true), Some(1));
        let chars: Vec<char> = "abcabc".chars().collect();
        assert_eq!(Pattern::new("b*", Options::default()).find(&chars, 0, false), Some((1, 6)));
        assert_eq!(Pattern::new("bc", Options::default()).find(&chars, 2, false), Some((4, 6)));
    }
}
