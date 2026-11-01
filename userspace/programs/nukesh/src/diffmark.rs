// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! git's diffs with the words that changed marked (ADR 0293):
//! `nukesh --diff-highlight` reads a diff git coloured and writes it
//! again, each line it removed and the line that took its place with the
//! words that differ in colors.toml's `diff.removed-word` and
//! `diff.added-word`; `nukesh --diff-pager` does the same into less(1).
//! The gitconfig Nukesh makes names the pager for `git diff`, `show` and
//! `log`, and the filter for `git add -p`, which needs a line out for
//! each line in.
//!
//! Only a run of removed lines followed by as many added ones, in a
//! change (after an `@@` line), is marked, and only where git coloured
//! them as plain removed and added lines: moved lines, combined diffs of
//! merges and diffs without colour pass through as they came. A pair
//! with most of the line changed is left whole, as marking all of it
//! says nothing.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use crate::colors::{Colors, Style};

/// Lines longer than this are left as they are: the comparison grows
/// with the square of the words.
const LONGEST: usize = 1000;

/// A pair is marked only if at most this share of its text changed.
const MOST_CHANGED: f64 = 0.6;

/// `nukesh --diff-highlight` and, with `pager`, `--diff-pager`. Returns
/// the exit status.
pub fn command(pager: bool) -> i32 {
    let home = std::env::var("HOME").ok();
    let (colors, _) = Colors::load(home.as_deref());
    let marks = Marks::new(&colors);
    let stdin = io::stdin().lock();
    if !pager {
        let mut out = io::BufWriter::new(io::stdout().lock());
        return status(mark(stdin, &mut out, &marks).and_then(|()| out.flush()));
    }
    // git sets LESS=FRX for its pager when it is not set; less is found
    // on PATH, and without it the diff goes out as it is.
    let child = Command::new("less").stdin(Stdio::piped()).spawn();
    let Ok(mut child) = child else {
        let mut out = io::BufWriter::new(io::stdout().lock());
        return status(mark(stdin, &mut out, &marks).and_then(|()| out.flush()));
    };
    let result = match child.stdin.take() {
        Some(pipe) => {
            let mut out = io::BufWriter::new(pipe);
            mark(stdin, &mut out, &marks).and_then(|()| out.flush())
        }
        None => Ok(()),
    };
    let waited = child.wait();
    match (result, waited) {
        // Quitting less early closes its end: not a failure.
        (Err(err), _) if err.kind() != io::ErrorKind::BrokenPipe => status(Err(err)),
        (_, Ok(exit)) => exit.code().unwrap_or(0),
        (_, Err(_)) => 1,
    }
}

fn status(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => 0,
        Err(err) => {
            eprintln!("nukesh: --diff-highlight: {err}");
            1
        }
    }
}

/// The colours: those git gives removed and added lines, which a line
/// must start with to be marked, and the shell's for them and their
/// words.
#[derive(Clone, Debug)]
pub struct Marks {
    removed: String,
    added: String,
    removed_word: String,
    added_word: String,
}

impl Marks {
    pub fn new(colors: &Colors) -> Marks {
        let sgr = |key: &str| sgr(colors.style(key));
        Marks {
            removed: sgr("diff.removed"),
            added: sgr("diff.added"),
            removed_word: sgr("diff.removed-word"),
            added_word: sgr("diff.added-word"),
        }
    }
}

fn sgr(style: Style) -> String {
    format!("\x1b[{}m", style.sgr())
}

/// A line of the diff: as it came, and without its colours.
struct Line {
    raw: Vec<u8>,
    plain: String,
}

/// Copies the diff from `input` to `out`, marking the words that
/// changed.
pub fn mark(input: impl Read, out: &mut impl Write, marks: &Marks) -> io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut in_change = false;
    let mut removed: Vec<Line> = Vec::new();
    let mut added: Vec<Line> = Vec::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let plain = strip(&String::from_utf8_lossy(&raw));
        let line = Line { raw: raw.clone(), plain };
        let first = line.plain.as_bytes().first().copied();
        if in_change && first == Some(b'-') {
            if !added.is_empty() {
                flush(out, &mut removed, &mut added, marks)?;
            }
            removed.push(line);
            continue;
        }
        if in_change && first == Some(b'+') {
            added.push(line);
            continue;
        }
        flush(out, &mut removed, &mut added, marks)?;
        // A change starts at its @@ line; a merge's combined diff, @@@,
        // is left alone.
        if line.plain.starts_with("@@ ") {
            in_change = true;
        } else if !matches!(first, Some(b' ' | b'\\')) {
            in_change = false;
        }
        out.write_all(&line.raw)?;
    }
    flush(out, &mut removed, &mut added, marks)
}

/// Writes a run of removed lines and the added ones after it: marked in
/// pairs if there are as many of each, as they came otherwise.
fn flush(
    out: &mut impl Write,
    removed: &mut Vec<Line>,
    added: &mut Vec<Line>,
    marks: &Marks,
) -> io::Result<()> {
    let pairs = removed.len() == added.len()
        && removed.iter().all(|l| starts_plain(l, &marks.removed))
        && added.iter().all(|l| starts_plain(l, &marks.added));
    let mut marked: Vec<Option<(String, String)>> = Vec::new();
    if pairs {
        for (old, new) in removed.iter().zip(added.iter()) {
            marked.push(mark_pair(&old.plain, &new.plain, marks));
        }
    }
    for (i, line) in removed.iter().enumerate() {
        match marked.get(i) {
            Some(Some((old, _))) => out.write_all(old.as_bytes())?,
            _ => out.write_all(&line.raw)?,
        }
    }
    for (i, line) in added.iter().enumerate() {
        match marked.get(i) {
            Some(Some((_, new))) => out.write_all(new.as_bytes())?,
            _ => out.write_all(&line.raw)?,
        }
    }
    removed.clear();
    added.clear();
    Ok(())
}

/// Whether git coloured `line` as a plain removed or added line: it
/// starts with that colour, and is not too long to compare.
fn starts_plain(line: &Line, color: &str) -> bool {
    line.raw.starts_with(color.as_bytes())
        && line.plain.len() <= LONGEST
        && std::str::from_utf8(&line.raw).is_ok()
}

/// `text` without its colours (SGR sequences).
fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The words of a line: runs of letters, digits and `_`, runs of
/// spaces, and each other character alone.
fn words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut last: Option<u8> = None;
    for (at, c) in text.char_indices() {
        let k = kind(c);
        if at > start && (Some(k) != last || k == 2) {
            words.push(&text[start..at]);
            start = at;
        }
        last = Some(k);
    }
    if start < text.len() {
        words.push(&text[start..]);
    }
    words
}

/// Which words of `a` and of `b` are kept between them: a longest
/// common subsequence.
fn kept(a: &[&str], b: &[&str]) -> (Vec<bool>, Vec<bool>) {
    let (n, m) = (a.len(), b.len());
    let mut table = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut in_a, mut in_b) = (vec![false; n], vec![false; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            in_a[i] = true;
            in_b[j] = true;
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    (in_a, in_b)
}

/// A removed line and the added one after it, each written again with
/// the words that changed marked; None to leave them as they came.
fn mark_pair(old: &str, new: &str, marks: &Marks) -> Option<(String, String)> {
    let (old, old_end) = split_end(old);
    let (new, new_end) = split_end(new);
    let (old_body, new_body) = (old.get(1..)?, new.get(1..)?);
    let (a, b) = (words(old_body), words(new_body));
    let (in_a, in_b) = kept(&a, &b);
    let changed = |words: &[&str], kept: &[bool]| -> usize {
        words
            .iter()
            .zip(kept)
            .filter(|(w, k)| !**k && !w.trim().is_empty())
            .map(|(w, _)| w.len())
            .sum()
    };
    let size = |words: &[&str]| -> usize { words.iter().map(|w| w.trim().len()).sum() };
    let (changed_a, changed_b) = (changed(&a, &in_a), changed(&b, &in_b));
    if changed_a + changed_b == 0 {
        return None;
    }
    for (changed, size) in [(changed_a, size(&a)), (changed_b, size(&b))] {
        if size > 0 && changed as f64 > MOST_CHANGED * size as f64 {
            return None;
        }
    }
    let write = |sign: char, words: &[&str], kept: &[bool], base: &str, word: &str, end: &str| {
        let mut line = format!("{base}{sign}");
        let mut marked = false;
        for (w, k) in words.iter().zip(kept) {
            // Changed spaces between kept words stay plain, so that a
            // re-indented line is not one long mark.
            let mark = !*k && !w.trim().is_empty();
            if mark != marked {
                line.push_str("\x1b[m");
                line.push_str(if mark { word } else { base });
                marked = mark;
            }
            line.push_str(w);
        }
        line.push_str("\x1b[m");
        line.push_str(end);
        line
    };
    Some((
        write('-', &a, &in_a, &marks.removed, &marks.removed_word, old_end),
        write('+', &b, &in_b, &marks.added, &marks.added_word, new_end),
    ))
}

/// A line, and its ending (`\n` or `\r\n`, or nothing at the end).
fn split_end(line: &str) -> (&str, &str) {
    let body = line.trim_end_matches(['\n', '\r']);
    (body, &line[body.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks() -> Marks {
        Marks::new(&Colors::default())
    }

    fn run(input: &str) -> String {
        let mut out = Vec::new();
        mark(input.as_bytes(), &mut out, &marks()).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn words_split_at_kinds_and_punctuation() {
        assert_eq!(
            words("let x = foo(1, 22);"),
            ["let", " ", "x", " ", "=", " ", "foo", "(", "1", ",", " ", "22", ")", ";"]
        );
        assert_eq!(words(""), Vec::<&str>::new());
    }

    #[test]
    fn a_changed_word_is_marked_in_both_lines() {
        let diff = "\x1b[1mdiff --git a/f b/f\x1b[m\n\x1b[36m@@ -1 +1 @@\x1b[m\n\
                    \x1b[31m-let x = 1;\x1b[m\n\x1b[32m+let x = 2;\x1b[m\n";
        let out = run(diff);
        assert!(
            out.contains("\x1b[31m-let x = \x1b[m\x1b[7;31m1\x1b[m\x1b[31m;\x1b[m\n"),
            "{out:?}"
        );
        assert!(
            out.contains("\x1b[32m+let x = \x1b[m\x1b[7;32m2\x1b[m\x1b[32m;\x1b[m\n"),
            "{out:?}"
        );
        assert!(out.starts_with("\x1b[1mdiff --git a/f b/f\x1b[m\n\x1b[36m@@ -1 +1 @@\x1b[m\n"));
    }

    #[test]
    fn as_many_lines_come_out_as_go_in() {
        let diff = "\x1b[36m@@ -1,3 +1,2 @@\x1b[m\n\x1b[31m-a b c\x1b[m\n\x1b[31m-d e f\x1b[m\n\
                    \x1b[32m+a b x\x1b[m\n context\n";
        let out = run(diff);
        assert_eq!(out.lines().count(), diff.lines().count());
        // Two removed and one added are not paired: as they came.
        assert_eq!(out, diff);
    }

    #[test]
    fn a_line_mostly_changed_is_left_whole() {
        let diff = "\x1b[36m@@ -1 +1 @@\x1b[m\n\x1b[31m-one two three\x1b[m\n\
                    \x1b[32m+four five six seven three\x1b[m\n";
        assert_eq!(run(diff), diff);
    }

    #[test]
    fn diffs_without_colour_and_moved_lines_pass_through() {
        let plain = "@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n";
        assert_eq!(run(plain), plain);
        let moved = "\x1b[36m@@ -1 +1 @@\x1b[m\n\x1b[1;35m-let x = 1;\x1b[m\n\
                     \x1b[1;36m+let x = 2;\x1b[m\n";
        assert_eq!(run(moved), moved);
    }

    #[test]
    fn headers_and_log_messages_are_not_changes() {
        // "--- a/f" and "+++ b/f" come before the @@ line; a commit's
        // message is indented.
        let diff = "\x1b[31m--- a/f\x1b[m\n\x1b[32m+++ b/f\x1b[m\ncommit 1\n\n    -x\n    +y\n";
        assert_eq!(run(diff), diff);
    }

    #[test]
    fn git_takes_the_shells_colours() {
        let colors = Colors::default();
        let config = colors.git_config();
        assert!(config.contains("[color \"diff\"]\n\tnew = green\n\told = red\n"), "{config}");
        assert!(config.contains("\tnewMoved = bold cyan\n\toldMoved = bold magenta\n"));
        assert!(config.contains("\tlog = nukesh --diff-pager\n"));
        assert!(config.contains("\tdiffFilter = nukesh --diff-highlight\n"));
        assert_eq!(Style::parse("bold bright-white on red").unwrap().git(), "bold brightwhite red");
        assert_eq!(Style::parse("on blue").unwrap().git(), "normal blue");
        assert_eq!(Style::parse("none").unwrap().git(), "normal");
        assert_eq!(Style::parse("red underline reverse").unwrap().git(), "ul reverse red");
    }
}
