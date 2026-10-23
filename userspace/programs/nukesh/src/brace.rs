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
                match sequence(&items[open + 1..close]) {
                    Some(seq) => {
                        seq.into_iter().map(|s| s.chars().map(Item::Char).collect()).collect()
                    }
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
                if commas.is_empty() && sequence(&items[open + 1..i]).is_none() {
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
fn sequence(items: &[Item]) -> Option<Vec<String>> {
    let mut text = String::new();
    for item in items {
        match item {
            Item::Char(c) => text.push(*c),
            Item::Part(_) => return None,
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
    let step = step.unsigned_abs().max(1) as i64;
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        // Zero padding when either end is written with a leading zero.
        let padded = |s: &str| {
            let digits = s.trim_start_matches('-');
            digits.len() > 1 && digits.starts_with('0')
        };
        let width = if padded(parts[0]) || padded(parts[1]) {
            parts[0].len().max(parts[1].len())
        } else {
            0
        };
        let mut out = Vec::new();
        let mut n = a;
        loop {
            out.push(if width > 0 {
                if n < 0 { format!("-{:0w$}", -n, w = width - 1) } else { format!("{n:0width$}") }
            } else {
                n.to_string()
            });
            if n == b || out.len() > 1_000_000 {
                break;
            }
            let next = if a <= b { n + step } else { n - step };
            if (a <= b && next > b) || (a > b && next < b) {
                break;
            }
            n = next;
        }
        return Some(out);
    }
    let (a, b) = (single_char(parts[0])?, single_char(parts[1])?);
    if !a.is_ascii_alphabetic() || !b.is_ascii_alphabetic() {
        return None;
    }
    let (a, b) = (a as i64, b as i64);
    let mut out = Vec::new();
    let mut n = a;
    loop {
        out.push(char::from_u32(n as u32)?.to_string());
        if n == b {
            break;
        }
        let next = if a <= b { n + step } else { n - step };
        if (a <= b && next > b) || (a > b && next < b) {
            break;
        }
        n = next;
    }
    Some(out)
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
