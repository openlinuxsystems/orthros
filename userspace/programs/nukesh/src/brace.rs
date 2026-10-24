// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Brace expansion: `a{b,c}d` is `abd acd`, `{1..5}` is `1 2 3 4 5`.
//!
//! It comes before every other expansion and works on the word as it was
//! written: only unquoted braces and commas count, and the parts between
//! them (quoted strings, `$x`, `$(...)`) are carried along whole, so
//! `{$a,"b c"}` is two words, `$a` and `"b c"`.

use crate::ast::{Word, WordPart};

/// A word as characters and the parts that are not plain text.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Char(char),
    /// A word of a sequence, which holds no brace or comma.
    Text(String),
    Part(WordPart),
}

/// The words `word` expands to: itself alone if it has no braces to
/// expand.
pub fn expand(word: &Word) -> Vec<Word> {
    let has_brace = word.parts.iter().any(|p| matches!(p, WordPart::Lit(s) if s.contains('{')));
    if !has_brace {
        return vec![word.clone()];
    }
    let mut items = Vec::new();
    for part in &word.parts {
        match part {
            WordPart::Lit(s) => items.extend(s.chars().map(Item::Char)),
            other => items.push(Item::Part(other.clone())),
        }
    }
    expand_items(&items, 0).into_iter().map(|items| to_word(&items)).collect()
}

/// How many braces are expanded inside braces and after them: those past
/// it are left as they are, so that each level's copy of the text stays
/// cheap and the stack shallow. (Every brace after another at least
/// doubles the words, so text past it could not be expanded anyway.)
const MAX_DEPTH: u32 = 256;

fn to_word(items: &[Item]) -> Word {
    let mut parts = Vec::new();
    let mut lit = String::new();
    for item in items {
        match item {
            Item::Char(c) => lit.push(*c),
            Item::Text(text) => lit.push_str(text),
            Item::Part(part) => {
                if !lit.is_empty() {
                    parts.push(WordPart::Lit(std::mem::take(&mut lit)));
                }
                parts.push(part.clone());
            }
        }
    }
    if !lit.is_empty() {
        parts.push(WordPart::Lit(lit));
    }
    Word { parts }
}

fn expand_items(items: &[Item], depth: u32) -> Vec<Vec<Item>> {
    if depth > MAX_DEPTH {
        return vec![items.to_vec()];
    }
    let mut start = 0;
    while let Some(open) = find_open(items, start) {
        if let Some((close, commas)) = find_close(items, open) {
            let pre = &items[..open];
            let post = &items[close + 1..];
            let alternatives: Vec<Vec<Item>> = if commas.is_empty() {
                match Sequence::of(&items[open + 1..close]) {
                    // The backslash of `{Z..b}` goes in quote removal, as
                    // in bash: an empty word of its own.
                    Some(seq) => seq
                        .words()
                        .into_iter()
                        .map(|s| match s.as_str() {
                            "\\" => vec![Item::Part(WordPart::Quoted(String::new()))],
                            _ => vec![Item::Text(s)],
                        })
                        .collect(),
                    None => {
                        start = open + 1;
                        continue;
                    }
                }
            } else {
                let mut bounds = vec![open];
                bounds.extend(&commas);
                bounds.push(close);
                bounds.windows(2).map(|w| items[w[0] + 1..w[1]].to_vec()).collect()
            };
            let posts = expand_items(post, depth + 1);
            let mut out = Vec::new();
            for alternative in alternatives {
                for expanded in expand_items(&alternative, depth + 1) {
                    for post in &posts {
                        let mut word = pre.to_vec();
                        word.extend(expanded.iter().cloned());
                        word.extend(post.iter().cloned());
                        out.push(word);
                    }
                }
            }
            return out;
        }
        start = open + 1;
    }
    vec![items.to_vec()]
}

/// The next `{` at or after `from` that is not `${`.
fn find_open(items: &[Item], from: usize) -> Option<usize> {
    (from..items.len())
        .find(|&i| items[i] == Item::Char('{') && !(i > 0 && items[i - 1] == Item::Char('$')))
}

/// The `}` closing the `{` at `open`, and the commas at its level; `None`
/// if it is not closed, or holds neither a comma nor a sequence.
fn find_close(items: &[Item], open: usize) -> Option<(usize, Vec<usize>)> {
    let mut depth = 0;
    let mut commas = Vec::new();
    for (i, item) in items.iter().enumerate().skip(open + 1) {
        match item {
            Item::Char('{') => depth += 1,
            Item::Char('}') if depth == 0 => {
                if commas.is_empty() && Sequence::of(&items[open + 1..i]).is_none() {
                    return None;
                }
                return Some((i, commas));
            }
            Item::Char('}') => depth -= 1,
            Item::Char(',') if depth == 0 => commas.push(i),
            _ => {}
        }
    }
    None
}

/// `a..b` or `a..b..step`, as numbers or single letters.
struct Sequence {
    a: i64,
    b: i64,
    step: i64,
    letters: bool,
    /// The width zero padding makes each number, when either end is
    /// written with a leading zero.
    width: usize,
}

impl Sequence {
    /// The sequence `items` spell, if they spell one bash expands: its
    /// increment's sign does not count, and an increment with no
    /// positive value (`-2^63`), ends too far apart for 64 bits, or more
    /// numbers than bash makes (2^31 - 3) are no sequence.
    fn of(items: &[Item]) -> Option<Sequence> {
        let mut text = String::new();
        for item in items {
            match item {
                Item::Char(c) => text.push(*c),
                Item::Text(_) | Item::Part(_) => return None,
            }
        }
        let parts: Vec<&str> = text.split("..").collect();
        if parts.len() != 2 && parts.len() != 3 {
            return None;
        }
        let step: i64 = match parts.get(2) {
            Some(step) => step.parse().ok()?,
            None => 1,
        };
        let step = step.checked_abs()?.max(1);
        if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
            let span = i128::from(b) - i128::from(a);
            if span < i128::from(i64::MIN) + 3 || span > i128::from(i64::MAX) - 2 {
                return None;
            }
            if span.unsigned_abs() / step as u128 > (i32::MAX - 3) as u128 {
                return None;
            }
            let padded = |s: &str| {
                let digits = s.trim_start_matches('-');
                digits.len() > 1 && digits.starts_with('0')
            };
            let width = if padded(parts[0]) || padded(parts[1]) {
                parts[0].len().max(parts[1].len())
            } else {
                0
            };
            return Some(Sequence { a, b, step, letters: false, width });
        }
        let (a, b) = (single_char(parts[0])?, single_char(parts[1])?);
        if !a.is_ascii_alphabetic() || !b.is_ascii_alphabetic() {
            return None;
        }
        Some(Sequence { a: a as i64, b: b as i64, step, letters: true, width: 0 })
    }

    fn words(&self) -> Vec<String> {
        let (a, b, step, width) = (self.a, self.b, self.step, self.width);
        let mut out = Vec::new();
        let mut n = a;
        loop {
            out.push(if self.letters {
                char::from_u32(n as u32).map(String::from).unwrap_or_default()
            } else if width == 0 {
                n.to_string()
            } else if n < 0 {
                format!("-{:0w$}", n.unsigned_abs(), w = width - 1)
            } else {
                format!("{n:0width$}")
            });
            // A next number past the end, or past what 64 bits hold, ends
            // the sequence.
            let next = if a <= b { n.checked_add(step) } else { n.checked_sub(step) };
            match next {
                Some(next) if n != b && ((a <= b && next <= b) || (a > b && next >= b)) => n = next,
                _ => break,
            }
        }
        out
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(word: &str) -> Vec<String> {
        expand(&Word::lit(word))
            .into_iter()
            .map(|w| w.as_literal().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn expands_lists() {
        assert_eq!(texts("a{b,c}d"), ["abd", "acd"]);
        assert_eq!(texts("{a,b}{1,2}"), ["a1", "a2", "b1", "b2"]);
        assert_eq!(texts("x{a,{b,c}}y"), ["xay", "xby", "xcy"]);
        assert_eq!(texts("{,a}"), ["", "a"]);
        assert_eq!(texts("pre{a,b}"), ["prea", "preb"]);
    }

    #[test]
    fn expands_sequences() {
        assert_eq!(texts("{1..4}"), ["1", "2", "3", "4"]);
        assert_eq!(texts("{3..1}"), ["3", "2", "1"]);
        assert_eq!(texts("{0..10..5}"), ["0", "5", "10"]);
        assert_eq!(texts("{01..03}"), ["01", "02", "03"]);
        assert_eq!(texts("{a..e..2}"), ["a", "c", "e"]);
        assert_eq!(texts("{-2..1}"), ["-2", "-1", "0", "1"]);
        assert_eq!(texts("{-05..3..4}"), ["-05", "-01", "003"]);
        assert_eq!(texts("{1..10..-3}"), ["1", "4", "7", "10"]);
    }

    #[test]
    fn sequences_stop_where_bash_stops_them() {
        // The next number would not fit in 64 bits.
        assert_eq!(texts("{9223372036854775806..9223372036854775807..5}"), ["9223372036854775806"]);
        assert_eq!(texts("{-9223372036854775807..-9223372036854775808}").len(), 2);
        // No increment, ends too far apart, too many numbers: no sequence.
        assert_eq!(texts("{1..3..-9223372036854775808}"), ["{1..3..-9223372036854775808}"]);
        assert_eq!(texts("{-9223372036854775808..0}"), ["{-9223372036854775808..0}"]);
        assert_eq!(texts("{1..2147483648}"), ["{1..2147483648}"]);
        // No cap below bash's.
        assert_eq!(texts("{1..3000000}").len(), 3_000_000);
        // The backslash of a run of letters is quote removal's.
        let words = expand(&Word::lit("{Z..b}"));
        assert_eq!(words.len(), 9);
        assert_eq!(words[2].parts, [WordPart::Quoted(String::new())]);
    }

    #[test]
    fn leaves_other_braces() {
        assert_eq!(texts("{}"), ["{}"]);
        assert_eq!(texts("{a}"), ["{a}"]);
        assert_eq!(texts("{a,b"), ["{a,b"]);
        assert_eq!(texts("${x}"), ["${x}"]);
        assert_eq!(texts("{1..a}"), ["{1..a}"]);
    }

    #[test]
    fn keeps_other_parts_whole() {
        let word = Word {
            parts: vec![
                WordPart::Lit("{".into()),
                WordPart::Quoted("x y".into()),
                WordPart::Lit(",z}".into()),
            ],
        };
        let words = expand(&word);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].parts, [WordPart::Quoted("x y".into())]);
        assert_eq!(words[1].as_literal(), Some("z"));
    }
}
