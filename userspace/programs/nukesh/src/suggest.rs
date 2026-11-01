// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! "Did you mean ...?" after "command not found" (ADR 0259).
//!
//! The name typed is compared with every alias, function, builtin and
//! program on `PATH`; the closest within two edits (one for a name of four
//! letters or fewer), where an edit is a letter added, removed, changed,
//! or two side by side swapped, is offered. Nothing is ever run in its
//! place.

use crate::shell::Shell;

/// The command most like `typed`, if one is close enough.
pub fn did_you_mean(shell: &Shell, typed: &str) -> Option<String> {
    let typed_len = typed.chars().count();
    if typed_len < 2 || typed.contains('/') {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    names.extend(shell.aliases.keys().cloned());
    names.extend(shell.functions.keys().cloned());
    names.extend(
        crate::builtins::TABLE
            .iter()
            .filter(|b| crate::builtins::enabled(shell, b.name))
            .map(|b| b.name.to_string()),
    );
    let path = shell.get("PATH").unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let Ok(entries) = std::fs::read_dir(crate::text::to_os(dir)) else { continue };
        for entry in entries.flatten() {
            let name = crate::text::from_os(&entry.file_name());
            if name.chars().count().abs_diff(typed_len) <= 2 {
                let full = format!("{}/{name}", dir.trim_end_matches('/'));
                if crate::sys::executable(&full) {
                    names.push(name);
                }
            }
        }
    }
    closest(typed, names)
}

/// The closest of `names` to `typed`: the fewest edits, then the same
/// first letter, then the first in order.
fn closest(typed: &str, mut names: Vec<String>) -> Option<String> {
    let most = if typed.chars().count() <= 4 { 1 } else { 2 };
    names.sort();
    names.dedup();
    let first = typed.chars().next();
    names
        .into_iter()
        .filter(|name| name != typed)
        .filter_map(|name| {
            let edits = distance(typed, &name);
            (edits <= most).then_some((edits, name.chars().next() != first, name))
        })
        .min()
        .map(|(_, _, name)| name)
}

/// The optimal string alignment distance between `a` and `b`: letters
/// added, removed or changed, and neighbours swapped, each one edit.
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn edits_are_counted() {
        assert_eq!(distance("sevice", "service"), 1);
        assert_eq!(distance("gerp", "grep"), 1);
        assert_eq!(distance("sl", "ls"), 1);
        assert_eq!(distance("pyhton", "python"), 1);
        assert_eq!(distance("abc", "abc"), 0);
        assert_eq!(distance("", "ab"), 2);
        assert_eq!(distance("kitten", "sitting"), 3);
    }

    #[test]
    fn the_closest_name_is_offered() {
        let list = names(&["service", "serve", "grep", "egrep", "ls", "less", "ln"]);
        assert_eq!(closest("sevice", list.clone()).as_deref(), Some("service"));
        assert_eq!(closest("gerp", list.clone()).as_deref(), Some("grep"));
        assert_eq!(closest("lss", list.clone()).as_deref(), Some("less"));
        // Too far from anything.
        assert_eq!(closest("xyzzy", list.clone()), None);
        // Short names allow one edit only.
        assert_eq!(closest("lxx", list), None);
    }
}
