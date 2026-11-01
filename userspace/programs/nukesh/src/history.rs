// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Command history: the list, its file, `!` expansion, and the `history`
//! and `fc` builtins.
//!
//! The file is bash's format with a `#seconds` line before each entry, so
//! that entries keep their times and multi-line commands stay whole.
//! Several shells may share it: each appends only its own new entries,
//! under an exclusive `flock`, in one write; trimming the file to
//! `HISTFILESIZE` writes a new file and renames it over the old one, and a
//! shell that waited for the lock on the old file opens the new one.

use std::cmp::Ordering;
use std::os::unix::fs::MetadataExt;

use crate::builtins::{flags, out};
use crate::pattern::{Options, Pattern};
use crate::shell::{Res, Shell};
use crate::{sys, text};

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub line: String,
    /// When it was entered, in seconds since 1970.
    pub time: i64,
}

#[derive(Debug, Default)]
pub struct History {
    pub entries: Vec<Entry>,
    /// The number of the first entry: entries dropped from the front
    /// keep the numbers of the rest.
    pub base: usize,
    /// Entries before this one are in the file already.
    saved: usize,
    /// Entries of the file read so far, for `history -n`.
    read_from_file: usize,
    /// `set -o history`.
    pub on: bool,
    /// `set -o histexpand`: `!` expansion.
    pub expand: bool,
    /// A subshell never writes the file.
    disabled: bool,
    /// Bumped when the list changes other than by adding, so that the line
    /// editor can take it again.
    pub generation: u64,
}

impl History {
    /// The number the next entry gets (`HISTCMD`, `\!`).
    /// The entries not yet in the file; none for a shell that never
    /// writes it.
    pub fn unsaved(&self) -> &[Entry] {
        if self.disabled { &[] } else { &self.entries[self.saved.min(self.entries.len())..] }
    }

    pub fn count(&self) -> usize {
        self.base + self.entries.len() + 1
    }

    pub fn disable(&mut self) {
        self.disabled = true;
        self.on = false;
    }

    fn number(&self, index: usize) -> usize {
        self.base + index + 1
    }

    /// Keeps the last `size` entries.
    fn trim(&mut self, size: usize) {
        if self.entries.len() > size {
            let drop = self.entries.len() - size;
            self.entries.drain(..drop);
            self.base += drop;
            self.saved = self.saved.saturating_sub(drop);
            self.generation += 1;
        }
    }
}

/// Parses a history file: an entry per line, but the lines after a time
/// line are one entry, up to the next time line (as readline reads them
/// with `lithist`), so that a multi-line command comes back whole. Lines
/// before the first time line, as in a file bash wrote without times
/// that this shell then added to, are an entry each.
fn parse_file(data: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut time = 0;
    // Whether a time line came, and the next line starts a timed entry.
    let mut timed = false;
    // Whether the lines that come are more of the last entry.
    let mut open = false;
    for line in data.lines() {
        if is_time_line(line) {
            time = line[1..].parse().unwrap_or(0);
            timed = true;
            open = false;
            continue;
        }
        if open {
            if let Some(last) = entries.last_mut() {
                last.line.push('\n');
                last.line.push_str(line);
            }
            continue;
        }
        entries.push(Entry { line: line.to_string(), time });
        open = core::mem::take(&mut timed);
    }
    entries
}

fn is_time_line(line: &str) -> bool {
    line.len() > 1 && line.starts_with('#') && line[1..].chars().all(|c| c.is_ascii_digit())
}

pub fn format_entries(entries: &[Entry]) -> String {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!("#{}\n{}\n", entry.time, entry.line));
    }
    out
}

/// Locks `path` (made if need be) for writing, following a rename
/// another shell made while this one waited.
fn lock_file(path: &str) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    for _ in 0..8 {
        // A new history file is the user's alone, as bash makes it.
        let file = std::fs::OpenOptions::new()
            .append(true)
            .read(true)
            .create(true)
            .mode(0o600)
            .open(text::to_os(path))
            .ok()?;
        // SAFETY: flock only acts on the descriptor.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        let same = match (file.metadata(), std::fs::metadata(text::to_os(path))) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if same {
            return Some(file);
        }
    }
    None
}

impl Shell {
    /// The history file, if history is kept in one.
    pub fn history_file(&self) -> Option<String> {
        self.get("HISTFILE").filter(|f| !f.is_empty())
    }

    fn history_size(&self, name: &str, default: usize) -> usize {
        match self.get(name) {
            Some(v) => v
                .trim()
                .parse::<i64>()
                .map_or(default, |n| if n < 0 { usize::MAX } else { n as usize }),
            None => default,
        }
    }

    /// Reads the history file at the start of an interactive shell.
    pub fn load_history(&mut self) {
        let Some(path) = self.history_file() else { return };
        let Ok(data) = std::fs::read(text::to_os(&path)) else { return };
        let entries = parse_file(&text::decode(&data));
        self.history.read_from_file = entries.len();
        self.history.entries = entries;
        let size = self.history_size("HISTSIZE", 500);
        self.history.trim(size);
        self.history.saved = self.history.entries.len();
        self.history.generation += 1;
    }

    /// Adds a command line to the history, as `HISTCONTROL` and
    /// `HISTIGNORE` allow.
    pub fn add_history(&mut self, line: &str) {
        if !self.history.on {
            return;
        }
        let line = line.trim_end_matches('\n');
        if line.trim().is_empty() {
            return;
        }
        let control = self.get("HISTCONTROL").unwrap_or_default();
        let has = |word: &str| {
            control.split(':').any(|c| c == word || (c == "ignoreboth" && word != "erasedups"))
        };
        if has("ignorespace") && line.starts_with(' ') {
            return;
        }
        if has("ignoredups") && self.history.entries.last().is_some_and(|e| e.line == line) {
            return;
        }
        if let Some(ignore) = self.get("HISTIGNORE") {
            let options = Options { extglob: self.shopt("extglob"), ..Options::default() };
            for pattern in ignore.split(':').filter(|p| !p.is_empty()) {
                let pattern = pattern.replace(
                    '&',
                    &self
                        .history
                        .entries
                        .last()
                        .map_or(String::new(), |e| crate::pattern::escape(&e.line)),
                );
                if Pattern::new(&pattern, options).matches(line) {
                    return;
                }
            }
        }
        if has("erasedups") {
            let before = self.history.entries.len();
            let saved = self.history.saved;
            let mut kept_saved = 0;
            let mut kept = Vec::new();
            for (i, entry) in self.history.entries.drain(..).enumerate() {
                if entry.line != line {
                    if i < saved {
                        kept_saved += 1;
                    }
                    kept.push(entry);
                }
            }
            self.history.entries = kept;
            self.history.saved = kept_saved;
            if self.history.entries.len() != before {
                self.history.generation += 1;
            }
        }
        // Multi-line commands are kept on one line unless `lithist`.
        let stored = if self.shopt("lithist") || !line.contains('\n') {
            line.to_string()
        } else {
            join_lines(line)
        };
        self.history.entries.push(Entry { line: stored, time: sys::now().0 });
        let size = self.history_size("HISTSIZE", 500);
        self.history.trim(size);
    }

    /// Writes the new entries to the history file, as an interactive
    /// shell does when it exits.
    pub fn save_history(&mut self) {
        if self.history.disabled || !self.interactive || self.private {
            return;
        }
        let Some(path) = self.history_file() else { return };
        if self.shopt("histappend") {
            self.append_history(&path);
        } else {
            self.write_history(&path);
        }
    }

    fn append_history(&mut self, path: &str) -> bool {
        use std::io::Write;
        let new = &self.history.entries[self.history.saved.min(self.history.entries.len())..];
        if new.is_empty() {
            return true;
        }
        let data = text::encode(&format_entries(new));
        let Some(mut file) = lock_file(path) else { return false };
        if file.write_all(&data).is_err() {
            return false;
        }
        self.history.saved = self.history.entries.len();
        let limit = self.history_size("HISTFILESIZE", self.history_size("HISTSIZE", 500));
        trim_file(path, &mut file, limit);
        true
    }

    /// Writes the whole list over the file.
    fn write_history(&mut self, path: &str) -> bool {
        let limit = self.history_size("HISTFILESIZE", self.history_size("HISTSIZE", 500));
        let start = self.history.entries.len().saturating_sub(limit);
        let data = text::encode(&format_entries(&self.history.entries[start..]));
        let Some(file) = lock_file(path) else { return false };
        let ok = replace_file(path, &data);
        drop(file);
        if ok {
            self.history.saved = self.history.entries.len();
        }
        ok
    }

    pub fn history_option(&mut self, name: &str, on: bool) {
        match name {
            "history" => self.history.on = on && !self.history.disabled,
            "histexpand" => self.history.expand = on,
            _ => {}
        }
    }

    pub fn history_option_on(&self, name: &str) -> bool {
        match name {
            "history" => self.history.on,
            "histexpand" => self.history.expand,
            _ => false,
        }
    }
}

/// A multi-line command on one line: `;` where the lines break, except
/// after a word that already ends a command or opens a list.
fn join_lines(line: &str) -> String {
    let joined = join_naively(line);
    // Joining must not change the command: a newline in quotes or in a
    // here-document stays.
    let same = match (crate::parse::parse(&joined, "history"), crate::parse::parse(line, "history"))
    {
        (Ok(a), Ok(b)) => crate::show::program_line(&a) == crate::show::program_line(&b),
        _ => false,
    };
    if same { joined } else { line.to_string() }
}

/// The lines of a command on one line, with `;` where one is needed.
fn join_naively(line: &str) -> String {
    let mut out = String::new();
    for (i, part) in line.split('\n').enumerate() {
        let part = part.trim_end();
        if i > 0 {
            let prev = out.trim_end();
            let opens = ["then", "do", "else", "{", "(", "|", "&&", "||", ";", "in", "&"]
                .iter()
                .any(|w| prev.ends_with(w));
            if !opens && !prev.is_empty() && !part.trim().is_empty() {
                out.push(';');
            }
            out.push(' ');
            out.push_str(part.trim_start());
        } else {
            out.push_str(part);
        }
    }
    out
}

/// Writes `data` to a new file and renames it over `path`.
fn replace_file(path: &str, data: &[u8]) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // A history file that is a symbolic link (dotfiles kept elsewhere) is
    // replaced where the link points, as readline writes through it, so
    // that the link stays and its target keeps the history.
    let path = std::fs::canonicalize(text::to_os(path))
        .map_or_else(|_| path.to_string(), |real| text::from_os(real.as_os_str()));
    let temp = format!("{path}.{}.tmp", sys::getpid());
    // Made new, and readable only by the user until it has the old file's
    // permissions: never another file, or a link, already at that name.
    // On the disk before the rename, so that a crash leaves the old file
    // or the new one, never an empty one.
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(text::to_os(&temp))
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        });
    if written.is_err() {
        let _ = std::fs::remove_file(text::to_os(&temp));
        return false;
    }
    if let Ok(meta) = std::fs::metadata(text::to_os(&path)) {
        let _ = std::fs::set_permissions(text::to_os(&temp), meta.permissions());
    }
    if std::fs::rename(text::to_os(&temp), text::to_os(&path)).is_err() {
        let _ = std::fs::remove_file(text::to_os(&temp));
        return false;
    }
    true
}

/// Trims the locked history file to its last `limit` entries.
fn trim_file(path: &str, file: &mut std::fs::File, limit: usize) {
    use std::io::{Read, Seek};
    let mut data = Vec::new();
    if file.seek(std::io::SeekFrom::Start(0)).is_err() || file.read_to_end(&mut data).is_err() {
        return;
    }
    let entries = parse_file(&text::decode(&data));
    if entries.len() <= limit {
        return;
    }
    let kept = &entries[entries.len() - limit..];
    replace_file(path, &text::encode(&format_entries(kept)));
}

// `!` expansion.

/// What `!` expansion made of a line.
pub enum Expanded {
    /// Nothing to expand.
    Same,
    /// The new line, to run (and show).
    Run(String),
    /// `:p`: the new line, to show and keep but not run.
    Print(String),
}

/// The words of a command line, as `!` word designators count them.
fn line_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                word.push(c);
            }
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    word.push(c);
                }
                '\\' => {
                    word.push(c);
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                ' ' | '\t' | '\n' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                ';' | '&' | '|' | '<' | '>' | '(' | ')' => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                    let mut op = c.to_string();
                    if let Some(&next) = chars.peek()
                        && (next == c || (c == '>' && next == '&'))
                    {
                        op.push(next);
                        chars.next();
                    }
                    words.push(op);
                }
                _ => word.push(c),
            },
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

impl Shell {
    /// Expands `!` references to the history in `line`.
    pub fn expand_history(&mut self, line: &str) -> Result<Expanded, String> {
        if !self.history.expand || !line.contains('!') && !line.starts_with('^') {
            return Ok(Expanded::Same);
        }
        let chars: Vec<char> = line.chars().collect();
        let mut out = String::new();
        let mut changed = false;
        let mut print = false;
        let mut i = 0;
        let mut in_single = false;
        let mut in_double = false;
        // `^old^new^`: the last command with old made new.
        if chars.first() == Some(&'^') {
            let rest: String = chars[1..].iter().collect();
            let mut parts = rest.splitn(3, '^');
            let old = parts.next().unwrap_or("");
            let new = parts.next().unwrap_or("");
            let tail = parts.next().unwrap_or("");
            let Some(last) = self.history.entries.last() else {
                return Err("^: event not found".into());
            };
            if !last.line.contains(old) || old.is_empty() {
                return Err(format!("{old}: substitution failed"));
            }
            return Ok(Expanded::Run(format!("{}{tail}", last.line.replacen(old, new, 1))));
        }
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() && chars[i + 1] == '!' {
                out.push('!');
                i += 2;
                changed = true;
                continue;
            }
            if c == '\'' && !in_double {
                in_single = !in_single;
            }
            if c == '"' {
                in_double = !in_double;
            }
            if c != '!' || in_single {
                out.push(c);
                i += 1;
                continue;
            }
            let next = chars.get(i + 1).copied();
            if next.is_none_or(|n| {
                matches!(n, ' ' | '\t' | '\n' | '=' | '(') || (in_double && n == '"')
            }) {
                out.push('!');
                i += 1;
                continue;
            }
            // The event.
            i += 1;
            let (event, rest_at) = self.history_event(&chars, i, &out)?;
            i = rest_at;
            // Word designators and modifiers.
            let words = line_words(&event);
            let mut selected = event.clone();
            let shortcut = chars.get(i).copied().filter(|c| matches!(c, '^' | '$' | '*' | '%'));
            if chars.get(i) == Some(&':') || shortcut.is_some() {
                if shortcut.is_none() {
                    i += 1;
                }
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_digit()
                        || matches!(chars[i], '^' | '$' | '*' | '-' | '%'))
                {
                    i += 1;
                    if shortcut.is_some() {
                        break;
                    }
                }
                let spec: String = chars[start..i].iter().collect();
                if spec.is_empty() {
                    i = start - usize::from(shortcut.is_none());
                } else {
                    selected = select_words(&words, &spec)
                        .ok_or_else(|| format!("{spec}: bad word specifier"))?;
                }
            }
            while chars.get(i) == Some(&':') {
                let Some(&m) = chars.get(i + 1) else { break };
                i += 2;
                match m {
                    'h' => {
                        if let Some(slash) = selected.rfind('/') {
                            selected.truncate(slash);
                        }
                    }
                    't' => {
                        if let Some(slash) = selected.rfind('/') {
                            selected = selected[slash + 1..].to_string();
                        }
                    }
                    'r' => {
                        if let Some(dot) = selected.rfind('.')
                            && !selected[dot..].contains('/')
                        {
                            selected.truncate(dot);
                        }
                    }
                    'e' => {
                        if let Some(dot) = selected.rfind('.')
                            && !selected[dot..].contains('/')
                        {
                            selected = selected[dot..].to_string();
                        }
                    }
                    'p' => print = true,
                    'q' => selected = format!("'{}'", selected.replace('\'', "'\\''")),
                    'x' => {
                        selected = line_words(&selected)
                            .iter()
                            .map(|w| format!("'{}'", w.replace('\'', "'\\''")))
                            .collect::<Vec<_>>()
                            .join(" ");
                    }
                    's' | 'g' | '&' => {
                        let global = m == 'g';
                        if global && chars.get(i) == Some(&'s') {
                            i += 1;
                        }
                        let delim = chars.get(i).copied().unwrap_or('/');
                        i += 1;
                        let mut old = String::new();
                        while i < chars.len() && chars[i] != delim {
                            old.push(chars[i]);
                            i += 1;
                        }
                        i += 1;
                        let mut new = String::new();
                        while i < chars.len() && chars[i] != delim {
                            new.push(chars[i]);
                            i += 1;
                        }
                        if i < chars.len() {
                            i += 1;
                        }
                        let new = new.replace('&', &old);
                        if old.is_empty() || !selected.contains(&old) {
                            return Err(format!("{old}: substitution failed"));
                        }
                        selected = if global {
                            selected.replace(&old, &new)
                        } else {
                            selected.replacen(&old, &new, 1)
                        };
                    }
                    _ => {
                        i -= 2;
                        break;
                    }
                }
            }
            out.push_str(&selected);
            changed = true;
        }
        if !changed {
            return Ok(Expanded::Same);
        }
        Ok(if print { Expanded::Print(out) } else { Expanded::Run(out) })
    }

    /// The event a `!` names, starting at `chars[at]`: its text and where
    /// the rest of the line starts.
    fn history_event(
        &self,
        chars: &[char],
        at: usize,
        line_so_far: &str,
    ) -> Result<(String, usize), String> {
        let entries = &self.history.entries;
        let not_found = |what: &str| format!("!{what}: event not found");
        let mut i = at;
        match chars.get(i) {
            Some('!') => {
                let last = entries.last().ok_or_else(|| not_found("!"))?;
                Ok((last.line.clone(), i + 1))
            }
            Some('#') => Ok((line_so_far.to_string(), i + 1)),
            Some(c) if c.is_ascii_digit() || *c == '-' => {
                let negative = *c == '-';
                if negative {
                    i += 1;
                }
                let start = i;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                let digits: String = chars[start..i].iter().collect();
                let n: usize = digits.parse().map_err(|_| not_found(&digits))?;
                let index = if negative {
                    entries.len().checked_sub(n)
                } else {
                    n.checked_sub(self.history.base + 1)
                };
                let entry = index.and_then(|i| entries.get(i)).ok_or_else(|| {
                    not_found(&format!("{}{digits}", if negative { "-" } else { "" }))
                })?;
                Ok((entry.line.clone(), i))
            }
            Some('?') => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '?' && chars[i] != '\n' {
                    i += 1;
                }
                let needle: String = chars[start..i].iter().collect();
                if chars.get(i) == Some(&'?') {
                    i += 1;
                }
                let entry = entries
                    .iter()
                    .rev()
                    .find(|e| e.line.contains(&needle))
                    .ok_or_else(|| not_found(&format!("?{needle}")))?;
                Ok((entry.line.clone(), i))
            }
            Some(c) if matches!(c, '$' | '^' | '*' | ':') => {
                // `!$` and its kin: words of the last command.
                let last = entries.last().ok_or_else(|| not_found(&c.to_string()))?;
                Ok((last.line.clone(), i))
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !matches!(
                        chars[i],
                        ' ' | '\t' | '\n' | ':' | ';' | '&' | '|' | '"' | '\'' | '(' | ')'
                    )
                {
                    i += 1;
                }
                let prefix: String = chars[start..i].iter().collect();
                let entry = entries
                    .iter()
                    .rev()
                    .find(|e| e.line.starts_with(&prefix))
                    .ok_or_else(|| not_found(&prefix))?;
                Ok((entry.line.clone(), i))
            }
        }
    }
}

/// The words of an event a designator picks: `0`, `n`, `^`, `$`, `*`,
/// `n-m`, `n*`, `n-`, `%`.
fn select_words(words: &[String], spec: &str) -> Option<String> {
    let last = words.len().checked_sub(1)?;
    let number = |s: &str| -> Option<usize> {
        match s {
            "^" => Some(1),
            "$" => Some(last),
            _ => s.parse().ok(),
        }
    };
    let range = |from: usize, to: usize| -> Option<String> {
        if from > to || to > last {
            return if from == to + 1 { Some(String::new()) } else { None };
        }
        Some(words[from..=to].join(" "))
    };
    match spec {
        "*" => range(1, last).or(Some(String::new())),
        "%" => None,
        _ if spec.ends_with('*') && spec.len() > 1 => range(number(&spec[..spec.len() - 1])?, last),
        _ if spec.ends_with('-') && spec.len() > 1 => {
            range(number(&spec[..spec.len() - 1])?, last.saturating_sub(1))
        }
        _ => match spec.find('-').filter(|&p| p > 0) {
            Some(dash) => range(number(&spec[..dash])?, number(&spec[dash + 1..])?),
            None => {
                let n = number(spec)?;
                words.get(n).cloned()
            }
        },
    }
}

// The builtins.

pub fn history(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "history", &args[1..], "cd:anrwps", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('c') {
        shell.history.entries.clear();
        shell.history.saved = 0;
        shell.history.generation += 1;
    }
    if let Some(offset) = flags.value('d') {
        let len = shell.history.entries.len();
        let base = shell.history.base;
        let index = |text: &str| -> Option<usize> {
            let n: i64 = text.parse().ok()?;
            if n < 0 {
                len.checked_sub(n.unsigned_abs() as usize)
            } else {
                (n as usize).checked_sub(base + 1)
            }
        };
        let (from, to) = match offset.rsplit_once('-').filter(|(a, _)| !a.is_empty()) {
            Some((a, b)) => (index(a), index(b)),
            None => (index(offset), index(offset)),
        };
        match (from, to) {
            (Some(from), Some(to)) if from <= to && to < len => {
                shell.history.entries.drain(from..=to);
                shell.history.saved = shell.history.saved.min(shell.history.entries.len());
                shell.history.generation += 1;
            }
            _ => {
                shell.error(&format!("history: {offset}: history position out of range"));
                return Ok(1);
            }
        }
        return Ok(0);
    }
    if flags.has('s') {
        let line = flags.rest.join(" ");
        // The `history -s` itself is not kept.
        shell.history.entries.pop();
        let was = shell.history.on;
        shell.history.on = true;
        shell.add_history(&line);
        shell.history.on = was;
        return Ok(0);
    }
    if flags.has('p') {
        let mut text = String::new();
        for arg in &flags.rest {
            match shell.expand_history(arg) {
                Ok(Expanded::Same) => text.push_str(arg),
                Ok(Expanded::Run(line) | Expanded::Print(line)) => text.push_str(&line),
                Err(message) => {
                    shell.error(&format!("history: {message}"));
                    return Ok(1);
                }
            }
            text.push('\n');
        }
        return Ok(i32::from(!out(shell, "history", &text)));
    }
    let file_op = ['a', 'n', 'r', 'w'].into_iter().find(|c| flags.has(*c));
    if let Some(op) = file_op {
        let path = flags.rest.first().cloned().or_else(|| shell.get("HISTFILE"));
        let Some(path) = path.filter(|p| !p.is_empty()) else {
            shell.error("history: no history file");
            return Ok(1);
        };
        let ok = match op {
            'a' => shell.append_history(&path),
            'w' => shell.write_history(&path),
            _ => {
                let data = std::fs::read(text::to_os(&path)).map(|d| parse_file(&text::decode(&d)));
                match data {
                    Ok(entries) => {
                        let start = if op == 'n' {
                            shell.history.read_from_file.min(entries.len())
                        } else {
                            0
                        };
                        shell.history.read_from_file = entries.len();
                        shell.history.entries.extend_from_slice(&entries[start..]);
                        shell.history.saved = shell.history.entries.len();
                        let size = shell.history_size("HISTSIZE", 500);
                        shell.history.trim(size);
                        shell.history.generation += 1;
                        true
                    }
                    Err(_) => op == 'n',
                }
            }
        };
        if !ok {
            shell.error(&format!("history: {path}: cannot write the history file"));
            return Ok(1);
        }
        return Ok(0);
    }
    if flags.has('c') {
        return Ok(0);
    }
    let count = match flags.rest.first() {
        Some(n) => match n.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => {
                shell.error(&format!("history: {n}: numeric argument required"));
                return Ok(1);
            }
        },
        None => None,
    };
    let entries = &shell.history.entries;
    let start = count.map_or(0, |n| entries.len().saturating_sub(n));
    let time_format = shell.get("HISTTIMEFORMAT");
    let mut text = String::new();
    for (i, entry) in entries.iter().enumerate().skip(start) {
        let when = time_format.as_deref().map_or_default(|f| sys::strftime(f, entry.time));
        text.push_str(&format!("{:5}  {when}{}\n", shell.history.number(i), entry.line));
    }
    Ok(i32::from(!out(shell, "history", &text)))
}

pub fn fc(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "fc", &args[1..], "e:lnrs", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    // The `fc` command itself is not one of the commands it works on.
    let own = shell.history.entries.last().is_some_and(|e| e.line.trim_start().starts_with("fc"));
    let entries: Vec<Entry> = if own {
        shell.history.entries[..shell.history.entries.len() - 1].to_vec()
    } else {
        shell.history.entries.clone()
    };
    let base = shell.history.base;
    let find = |spec: &str| -> Option<usize> {
        if let Ok(n) = spec.parse::<i64>() {
            return match n.cmp(&0) {
                Ordering::Less => entries.len().checked_sub(n.unsigned_abs() as usize),
                Ordering::Equal => entries.len().checked_sub(1),
                Ordering::Greater => (n as usize)
                    .checked_sub(base + 1)
                    .map(|i| i.min(entries.len().saturating_sub(1))),
            };
        }
        entries.iter().rposition(|e| e.line.starts_with(spec))
    };
    if flags.has('s') {
        let mut rest = flags.rest.clone();
        let substitution = rest.first().filter(|r| r.contains('=')).cloned();
        if substitution.is_some() {
            rest.remove(0);
        }
        let index = match rest.first() {
            Some(spec) => find(spec),
            None => entries.len().checked_sub(1),
        };
        let Some(index) = index else {
            shell.error("fc: no command found");
            return Ok(1);
        };
        let mut line = entries[index].line.clone();
        if let Some(sub) = substitution
            && let Some((old, new)) = sub.split_once('=')
        {
            line = line.replace(old, new);
        }
        let _ = sys::write_all(2, &text::encode(&format!("{line}\n")));
        if own {
            shell.history.entries.pop();
        }
        shell.add_history(&line);
        return shell.eval_text(&line, "fc", "fc");
    }
    if entries.is_empty() {
        shell.error("fc: no command found");
        return Ok(1);
    }
    let last = entries.len() - 1;
    let (from, to) = match (flags.rest.first(), flags.rest.get(1)) {
        (None, _) if flags.has('l') => (last.saturating_sub(15), last),
        (None, _) => (last, last),
        (Some(a), None) if flags.has('l') => (find(a).unwrap_or(0), last),
        (Some(a), None) => {
            let i = find(a).unwrap_or(last);
            (i, i)
        }
        (Some(a), Some(b)) => (find(a).unwrap_or(0), find(b).unwrap_or(last)),
    };
    let mut range: Vec<usize> =
        if from <= to { (from..=to).collect() } else { (to..=from).rev().collect() };
    if flags.has('r') {
        range.reverse();
    }
    if flags.has('l') {
        let mut text = String::new();
        for i in range {
            if flags.has('n') {
                text.push_str(&format!("\t {}\n", entries[i].line));
            } else {
                text.push_str(&format!("{}\t {}\n", base + i + 1, entries[i].line));
            }
        }
        return Ok(i32::from(!out(shell, "fc", &text)));
    }
    // Edit the commands, then run them.
    let editor = flags
        .value('e')
        .map(str::to_string)
        .or_else(|| shell.get("FCEDIT"))
        .or_else(|| shell.get("EDITOR"))
        .unwrap_or_else(|| "nano".into());
    let mut commands = String::new();
    for &i in &range {
        commands.push_str(&entries[i].line);
        commands.push('\n');
    }
    let dir = shell.get("TMPDIR").filter(|dir| !dir.is_empty()).unwrap_or_else(|| "/tmp".into());
    let (pid, nanos) = (sys::getpid(), sys::now().1);
    let names =
        (0..100).map(|n| format!("{}/nukesh-fc-{pid}-{nanos}-{n}", dir.trim_end_matches('/')));
    let Some(path) = make_edit_file(names, &text::encode(&commands)) else {
        shell.error(&format!("fc: cannot make a file to edit in {dir}"));
        return Ok(1);
    };
    if own {
        shell.history.entries.pop();
    }
    let status =
        shell.eval_text(&format!("{editor} {}", crate::expand::quote(&path)), "fc", "fc")?;
    let edited = std::fs::read(text::to_os(&path)).map(|d| text::decode(&d));
    let _ = std::fs::remove_file(text::to_os(&path));
    if status != 0 || editor == "-" {
        return Ok(status);
    }
    let Ok(edited) = edited else { return Ok(1) };
    let _ = sys::write_all(2, &text::encode(&edited));
    shell.add_history(edited.trim_end());
    shell.eval_text(&edited, "fc", "fc")
}

/// Makes the file `fc` hands the editor, holding `data`, at the first of
/// `names` where nothing is yet, as mkstemp does: made new, so that a file
/// or a symbolic link someone put at the name is never written through,
/// and readable by its owner alone, as the commands may hold secrets.
fn make_edit_file(names: impl Iterator<Item = String>, data: &[u8]) -> Option<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    for path in names {
        let made = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(text::to_os(&path));
        match made {
            Ok(mut file) => {
                if file.write_all(data).is_err() {
                    drop(file);
                    let _ = std::fs::remove_file(text::to_os(&path));
                    return None;
                }
                return Some(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fc_never_writes_through_what_is_at_its_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("nukesh-fc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let taken = dir.join("a");
        std::os::unix::fs::symlink(&victim, &taken).unwrap();
        let names = ["a", "b"].map(|n| dir.join(n).to_string_lossy().into_owned());
        let path = make_edit_file(names.clone().into_iter(), b"echo secret\n").unwrap();
        assert_eq!(path, dir.join("b").to_string_lossy());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo secret\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        // With every name taken, there is no file.
        assert!(make_edit_file(names.into_iter(), b"").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_linked_history_file_is_replaced_where_it_points() {
        let dir = std::env::temp_dir().join(format!("nukesh-histlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(&real, "old\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(replace_file(&link.to_string_lossy(), b"new\n"));
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_files_keep_times_and_lines() {
        let entries = vec![
            Entry { line: "ls".into(), time: 10 },
            Entry { line: "for i in 1 2\ndo echo $i\ndone".into(), time: 20 },
        ];
        assert_eq!(parse_file(&format_entries(&entries)), entries);
        // A file without times has a command a line.
        assert_eq!(parse_file("a\nb\n").len(), 2);
        // One that bash wrote without times and this shell added to keeps
        // the old commands apart.
        let mixed = parse_file("ls\ncd /\necho old\n#30\nfor i in 1\ndo :\ndone\n");
        let lines: Vec<_> = mixed.iter().map(|e| (e.line.as_str(), e.time)).collect();
        assert_eq!(
            lines,
            [("ls", 0), ("cd /", 0), ("echo old", 0), ("for i in 1\ndo :\ndone", 30)]
        );
    }

    #[test]
    fn word_designators() {
        let words = line_words("cp a.txt /tmp/b && ls");
        assert_eq!(words, ["cp", "a.txt", "/tmp/b", "&&", "ls"]);
        assert_eq!(select_words(&words, "$").unwrap(), "ls");
        assert_eq!(select_words(&words, "1").unwrap(), "a.txt");
        assert_eq!(select_words(&words, "1-2").unwrap(), "a.txt /tmp/b");
        assert_eq!(select_words(&words, "*").unwrap(), "a.txt /tmp/b && ls");
    }

    #[test]
    fn bang_expansion() {
        let mut shell = Shell::new("test", false);
        shell.history.expand = true;
        shell.history.entries.push(Entry { line: "echo one two".into(), time: 0 });
        let run = |shell: &mut Shell, line: &str| match shell.expand_history(line) {
            Ok(Expanded::Run(s)) => s,
            Ok(Expanded::Same) => "same".into(),
            Ok(Expanded::Print(s)) => format!("print {s}"),
            Err(e) => format!("error {e}"),
        };
        assert_eq!(run(&mut shell, "sudo !!"), "sudo echo one two");
        assert_eq!(run(&mut shell, "ls !$"), "ls two");
        assert_eq!(run(&mut shell, "echo hi!"), "same");
        assert_eq!(run(&mut shell, "echo '!!'"), "same");
        assert_eq!(run(&mut shell, "!ec:s/one/1/"), "echo 1 two");
        assert_eq!(run(&mut shell, "^one^1"), "echo 1 two");
        assert_eq!(run(&mut shell, "!nope"), "error !nope: event not found");
    }

    #[test]
    fn multi_line_commands_join() {
        assert_eq!(join_lines("for i in 1 2\ndo echo $i\ndone"), "for i in 1 2; do echo $i; done");
        assert_eq!(join_lines("if true; then\necho a\nfi"), "if true; then echo a; fi");
        // Here-documents and quoted newlines keep their lines.
        assert_eq!(join_lines("cat <<EOF\nbody\nEOF"), "cat <<EOF\nbody\nEOF");
        assert_eq!(join_lines("echo 'a\nb'"), "echo 'a\nb'");
    }
}
