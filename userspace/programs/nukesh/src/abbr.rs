// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Abbreviations (ADR 0262), as fish has them: `abbr gs git status`, and
//! `gs` typed as a command grows into `git status` when Space or Enter
//! follows it, so what runs is seen first and lands in the history whole.
//! Unlike an alias, it is only in the line editor: scripts never see one.

use crate::shell::{Res, Shell};

/// `abbr` lists them; `abbr name expansion ...` sets one; `abbr -r name
/// ...` removes them.
pub fn abbr(shell: &mut Shell, args: &[String]) -> Res {
    let args = &args[1..];
    match args.first().map(String::as_str) {
        None => {
            for (name, expansion) in &shell.abbrs {
                println!("abbr {name} {}", crate::expand::quote_always(expansion));
            }
            Ok(0)
        }
        Some("-h" | "--help") => {
            println!("usage: abbr [name [expansion ...]] or abbr -r name ...");
            Ok(0)
        }
        Some("-r") => {
            let mut status = 0;
            for name in &args[1..] {
                if shell.abbrs.shift_remove(name).is_none() {
                    shell.error(&format!("abbr: {name}: not found"));
                    status = 1;
                }
            }
            Ok(status)
        }
        Some(name) if !valid(name) => {
            shell.error(&format!("abbr: '{name}': not a name an abbreviation can have"));
            Ok(2)
        }
        Some(name) if args.len() == 1 => match shell.abbrs.get(name) {
            Some(expansion) => {
                println!("abbr {name} {}", crate::expand::quote_always(expansion));
                Ok(0)
            }
            None => {
                shell.error(&format!("abbr: {name}: not found"));
                Ok(1)
            }
        },
        Some(name) => {
            let expansion = args[1..].join(" ");
            shell.abbrs.insert(name.to_string(), expansion);
            Ok(0)
        }
    }
}

fn valid(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name.chars().all(|c| !c.is_whitespace() && !"|&;<>()$`\\\"'=".contains(c))
}

/// Where in `line` the word ending at `end` starts, if it is in a
/// command's place ([`command_position`]).
fn command_word(line: &str, end: usize) -> Option<usize> {
    let before = &line[..end];
    let start =
        before.rfind(|c: char| c.is_whitespace() || "|&;()".contains(c)).map_or(0, |i| i + 1);
    command_position(line, start).then_some(start)
}

/// Whether a word starting at `start` in `line` is in a command's place:
/// the line's start, or after `|`, `;`, `&`, `(`, `!`, or a reserved word
/// that runs a command next, or `sudo`, `elevate` or `doas`. Reedline
/// asks this before it grows a word at Space (editor.rs).
pub fn command_position(line: &str, start: usize) -> bool {
    let Some(before) = line.get(..start) else { return false };
    if before.ends_with(['|', '&', ';', '(']) {
        return true;
    }
    let mut rest = before.trim_end();
    loop {
        if rest.is_empty() || rest.ends_with(['|', '&', ';', '(', '!']) {
            return true;
        }
        let last = rest.rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
        if matches!(last, "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "time") {
            return true;
        }
        // `sudo gs` and `elevate gs` grow too.
        if matches!(last, "sudo" | "elevate" | "doas") {
            rest = rest[..rest.len() - last.len()].trim_end();
            continue;
        }
        return false;
    }
}

/// The line with the abbreviation that ends at `end` grown, and where
/// the cursor goes after it; none if nothing grows.
pub fn grow(shell: &Shell, line: &str, end: usize) -> Option<(String, usize)> {
    if shell.abbrs.is_empty() || !line.is_char_boundary(end) {
        return None;
    }
    let start = command_word(line, end)?;
    let word = &line[start..end];
    let expansion = shell.abbrs.get(word)?;
    let mut out = String::with_capacity(line.len() + expansion.len());
    out.push_str(&line[..start]);
    out.push_str(expansion);
    let cursor = out.len();
    out.push_str(&line[end..]);
    Some((out, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> Shell {
        let mut shell = Shell::new("test", false);
        shell.abbrs.insert("gs".into(), "git status".into());
        shell
    }

    #[test]
    fn a_command_word_grows() {
        let shell = shell();
        assert_eq!(grow(&shell, "gs", 2), Some(("git status".into(), 10)));
        assert_eq!(grow(&shell, "ls | gs", 7), Some(("ls | git status".into(), 15)));
        assert_eq!(grow(&shell, "sudo gs", 7), Some(("sudo git status".into(), 15)));
        assert_eq!(grow(&shell, "if gs", 5), Some(("if git status".into(), 13)));
    }

    #[test]
    fn an_argument_does_not() {
        let shell = shell();
        assert_eq!(grow(&shell, "echo gs", 7), None);
        assert_eq!(grow(&shell, "gsx", 3), None);
        assert_eq!(grow(&shell, "g", 1), None);
    }

    #[test]
    fn names_are_checked() {
        assert!(valid("gs"));
        assert!(!valid("-x"));
        assert!(!valid("a b"));
        assert!(!valid("a|b"));
    }
}
