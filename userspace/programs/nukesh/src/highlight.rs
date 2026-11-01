// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Colouring the command line as it is typed (ADR 0258).
//!
//! The line is split as the shell would read it, closely enough to
//! colour it: commands, reserved words, options, quoted text, variables,
//! operators and redirections, and comments. A command where one starts
//! is looked for as an alias, a function, a builtin, a reserved word or a
//! program; one that is none of them is red and underlined, so that the
//! mistake shows before Enter, and without colour as well.
//!
//! This is not the parser: it never fails, and text it cannot place is
//! left plain.

/// What a piece of the line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
    Command,
    Unknown,
    Keyword,
    Option,
    String,
    Variable,
    Operator,
    Comment,
}

impl Role {
    /// Its key in colors.toml's `[line]`, or none for plain text.
    pub fn key(self) -> Option<&'static str> {
        Some(match self {
            Role::Plain => return None,
            Role::Command => "line.command",
            Role::Unknown => "line.unknown",
            Role::Keyword => "line.keyword",
            Role::Option => "line.option",
            Role::String => "line.string",
            Role::Variable => "line.variable",
            Role::Operator => "line.operator",
            Role::Comment => "line.comment",
        })
    }
}

/// Reserved words after which a command starts again.
const COMMAND_AFTER: &[&str] =
    &["!", "{", "do", "elif", "else", "if", "then", "until", "while", "time", "coproc", "(", "(("];

/// Reserved words that end a construct; what follows them is not a
/// command until an operator.
const ENDERS: &[&str] = &["}", "done", "esac", "fi", "]]"];

/// Splits `line` into pieces and what each is. `known` says whether a
/// command name, as typed and unquoted, runs something.
pub fn spans(line: &str, known: &mut dyn FnMut(&str) -> bool) -> Vec<(Role, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<(Role, String)> = Vec::new();
    let mut push = |role: Role, text: String| {
        if text.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((last, buf)) if *last == role => buf.push_str(&text),
            _ => out.push((role, text)),
        }
    };
    let mut i = 0;
    // Whether the next word is where a command starts.
    let mut at_command = true;
    // `for`, `case` and `select` take a name, then `in`.
    let mut wants_in = false;
    // Inside `[[ ]]`: `(`, `<` and `||` are its own, and no command starts.
    let mut in_cond = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            push(Role::Plain, "\n".into());
            at_command = true;
            wants_in = false;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            push(Role::Plain, c.to_string());
            i += 1;
            continue;
        }
        if c == '#' {
            let end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
            push(Role::Comment, chars[i..end].iter().collect());
            i = end;
            continue;
        }
        // Operators and redirections.
        if let Some(len) = operator_len(&chars[i..]) {
            let text: String = chars[i..i + len].iter().collect();
            let redirect = text.contains('<') || text.contains('>');
            push(Role::Operator, text.clone());
            i += len;
            if in_cond {
                continue;
            }
            if !redirect {
                at_command = !matches!(text.as_str(), ")" | "))");
                wants_in = false;
            } else {
                // The redirection's file: a plain word.
                while i < chars.len() && chars[i] == ' ' {
                    push(Role::Plain, " ".into());
                    i += 1;
                }
                let end = word_end(&chars, i);
                let saved = at_command;
                pieces_of_word(&chars[i..end], &mut push);
                at_command = saved;
                i = end;
            }
            continue;
        }
        // A word.
        let end = word_end(&chars, i);
        let word: String = chars[i..end].iter().collect();
        if in_cond && word == "]]" {
            push(Role::Keyword, word);
            in_cond = false;
            i = end;
            continue;
        }
        // An array, `a=(red green)` or `declare -A m=([k]=v)`: its words
        // are values.
        if word.ends_with('=')
            && chars.get(end) == Some(&'(')
            && assignment_name_len(&word).is_some()
        {
            push(Role::Variable, word);
            let close = close_paren(&chars, end + 1).min(chars.len());
            push(Role::Operator, "(".into());
            pieces_of_word(&chars[end + 1..close], &mut push);
            if close < chars.len() {
                push(Role::Operator, ")".into());
            }
            i = (close + 1).min(chars.len());
            continue;
        }
        if at_command {
            if let Some(eq) = assignment_name_len(&word) {
                push(Role::Variable, chars[i..=i + eq].iter().collect());
                pieces_of_word(&chars[i + eq + 1..end], &mut push);
                i = end;
                continue;
            }
            if crate::parse::is_reserved(&word) || word == "((" {
                push(Role::Keyword, word.clone());
                at_command = COMMAND_AFTER.contains(&word.as_str());
                wants_in = matches!(word.as_str(), "for" | "case" | "select");
                if word == "function" {
                    at_command = false;
                }
                if word == "[[" {
                    in_cond = true;
                }
                i = end;
                continue;
            }
            // History expansion, `!!`, `!$`, `^old^new`, runs what it names.
            let history = (word.starts_with('!') && word.len() > 1) || word.starts_with('^');
            let plain_name = !history && !word.contains(['\'', '"', '$', '`', '\\']);
            let role = if !plain_name || known(&word) { Role::Command } else { Role::Unknown };
            push(role, word);
            at_command = false;
            i = end;
            continue;
        }
        if wants_in && (word == "in" || word == "do") {
            push(Role::Keyword, word.clone());
            wants_in = false;
            at_command = word == "do";
            i = end;
            continue;
        }
        if ENDERS.contains(&word.as_str()) && out_last_is_separator(&chars[..i]) {
            push(Role::Keyword, word);
            i = end;
            continue;
        }
        if word.starts_with('-') && word.len() > 1 {
            push(Role::Option, word);
        } else {
            pieces_of_word(&chars[i..end], &mut push);
        }
        i = end;
    }
    out
}

/// Whether the text before a word ends a command: `;`, `&`, a newline or
/// nothing, so that `fi` or `done` there is a reserved word.
fn out_last_is_separator(before: &[char]) -> bool {
    let last = before.iter().rev().find(|c| **c != ' ' && **c != '\t');
    matches!(last, None | Some(';' | '&' | '\n' | '|' | '{' | '}'))
}

/// The length of the operator at the start of `chars`, if one is there:
/// control operators, and redirections with their file descriptor.
fn operator_len(chars: &[char]) -> Option<usize> {
    let digits = chars.iter().take_while(|c| c.is_ascii_digit()).count();
    let rest: String = chars[digits..].iter().take(3).collect();
    if digits > 0 && !(rest.starts_with('<') || rest.starts_with('>')) {
        return None;
    }
    const OPS: &[&str] = &[
        "<<<", "<<-", "&>>", ";;&", "&&", "||", ";;", ";&", "|&", "<<", ">>", "<&", ">&", "<>",
        ">|", "&>", "((", "))", "|", "&", ";", "<", ">", "(", ")",
    ];
    OPS.iter().find(|op| rest.starts_with(**op)).map(|op| digits + op.chars().count())
}

/// Where the word starting at `start` ends: at unquoted space or an
/// operator, after quotes and `$(...)`.
fn word_end(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => break,
            '\\' => i += 2,
            '\'' => i = close(chars, i + 1, '\'') + 1,
            '"' => i = close_double(chars, i + 1) + 1,
            '`' => i = close(chars, i + 1, '`') + 1,
            '$' if chars.get(i + 1) == Some(&'(') => i = close_paren(chars, i + 2) + 1,
            '$' if chars.get(i + 1) == Some(&'{') => i = close(chars, i + 2, '}') + 1,
            ';' | '&' | '|' | '<' | '>' | ')' => break,
            '(' if i > start => break,
            '(' => {
                i += 1;
                break;
            }
            _ => i += 1,
        }
    }
    i.min(chars.len()).max(start + 1).min(chars.len())
}

/// The index of `quote` closing a quote opened before `from`, or the end.
fn close(chars: &[char], from: usize, quote: char) -> usize {
    chars[from.min(chars.len())..]
        .iter()
        .position(|&c| c == quote)
        .map_or(chars.len(), |p| from + p)
}

fn close_double(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '"' => return i,
            _ => i += 1,
        }
    }
    chars.len()
}

fn close_paren(chars: &[char], from: usize) -> usize {
    let mut depth = 1;
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => i = close(chars, i + 1, '\''),
            '"' => i = close_double(chars, i + 1),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// `NAME=` at the start of `word`: the length of NAME.
fn assignment_name_len(word: &str) -> Option<usize> {
    let eq = word.find('=')?;
    let name = &word[..eq];
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(word[..eq].chars().count())
}

/// The pieces of a word that is not a command: quoted text, variables and
/// substitutions, and plain text.
fn pieces_of_word(chars: &[char], push: &mut dyn FnMut(Role, String)) {
    let mut i = 0;
    let mut plain = String::new();
    let flush = |plain: &mut String, push: &mut dyn FnMut(Role, String)| {
        if !plain.is_empty() {
            push(Role::Plain, std::mem::take(plain));
        }
    };
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                plain.extend(chars[i..(i + 2).min(chars.len())].iter());
                i += 2;
            }
            '\'' => {
                flush(&mut plain, push);
                let end = (close(chars, i + 1, '\'') + 1).min(chars.len());
                push(Role::String, chars[i..end].iter().collect());
                i = end;
            }
            '"' => {
                flush(&mut plain, push);
                let end = (close_double(chars, i + 1) + 1).min(chars.len());
                double_quoted(&chars[i..end], push);
                i = end;
            }
            '$' | '`' => {
                flush(&mut plain, push);
                let end = variable_end(chars, i);
                push(Role::Variable, chars[i..end].iter().collect());
                i = end;
            }
            c => {
                plain.push(c);
                i += 1;
            }
        }
    }
    flush(&mut plain, push);
}

/// `"..."`: a string, with the variables in it coloured as variables.
fn double_quoted(chars: &[char], push: &mut dyn FnMut(Role, String)) {
    let mut i = 0;
    let mut text = String::new();
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                text.extend(chars[i..(i + 2).min(chars.len())].iter());
                i += 2;
            }
            '$' | '`' if i + 1 < chars.len() => {
                let end = variable_end(chars, i).min(chars.len());
                if end > i + 1 {
                    push(Role::String, std::mem::take(&mut text));
                    push(Role::Variable, chars[i..end].iter().collect());
                    i = end;
                } else {
                    text.push(chars[i]);
                    i += 1;
                }
            }
            c => {
                text.push(c);
                i += 1;
            }
        }
    }
    push(Role::String, text);
}

/// Where the expansion at `start` (`$` or a backquote) ends.
fn variable_end(chars: &[char], start: usize) -> usize {
    if chars[start] == '`' {
        return (close(chars, start + 1, '`') + 1).min(chars.len());
    }
    let end = match chars.get(start + 1) {
        Some('(') => close_paren(chars, start + 2) + 1,
        Some('{') => close(chars, start + 2, '}') + 1,
        Some('\'') => close(chars, start + 2, '\'') + 1,
        Some(c) if c.is_ascii_digit() || "@*#?$!-".contains(*c) => start + 2,
        Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
            start
                + 1
                + chars[start + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                    .count()
        }
        _ => start + 1,
    };
    end.min(chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(line: &str) -> Vec<(Role, String)> {
        spans(line, &mut |name| matches!(name, "ls" | "grep" | "echo" | "./deploy.sh" | "head"))
    }

    fn joined(line: &str) -> String {
        roles(line).into_iter().map(|(_, t)| t).collect()
    }

    #[test]
    fn the_text_is_all_there() {
        for line in [
            "ls -l",
            "echo \"Deploying to $HOST\" && ./deploy.sh --dry-run",
            "grep -rn 'listen' site/ | head -2",
            "for f in *.txt; do echo $f; done # loop",
            "x=1 y=\"$(date +%s)\" echo ${x:-z} 2>/dev/null >&2",
            "echo 'unclosed",
            "echo \"unclosed $x",
            "echo $(",
            "a\\ b c\\",
            "",
        ] {
            assert_eq!(joined(line), line);
        }
    }

    #[test]
    fn commands_known_and_unknown() {
        assert_eq!(roles("ls -l")[0], (Role::Command, "ls".into()));
        assert_eq!(roles("ls -l")[2], (Role::Option, "-l".into()));
        assert_eq!(roles("sevice status nginx")[0], (Role::Unknown, "sevice".into()));
        let piped = roles("ls | grp x");
        assert!(piped.contains(&(Role::Operator, "|".into())));
        assert!(piped.contains(&(Role::Unknown, "grp".into())));
        // Quoted or expanded names are not judged.
        assert_eq!(roles("\"$cmd\" x")[0].0, Role::Command);
    }

    #[test]
    fn strings_variables_and_operators() {
        let spans = roles("echo \"Deploying to $HOST\" && ./deploy.sh --dry-run");
        assert_eq!(
            spans,
            vec![
                (Role::Command, "echo".into()),
                (Role::Plain, " ".into()),
                (Role::String, "\"Deploying to ".into()),
                (Role::Variable, "$HOST".into()),
                (Role::String, "\"".into()),
                (Role::Plain, " ".into()),
                (Role::Operator, "&&".into()),
                (Role::Plain, " ".into()),
                (Role::Command, "./deploy.sh".into()),
                (Role::Plain, " ".into()),
                (Role::Option, "--dry-run".into()),
            ]
        );
    }

    #[test]
    fn reserved_words_and_comments() {
        let spans = roles("for f in a b; do echo $f; done # x");
        assert_eq!(spans[0], (Role::Keyword, "for".into()));
        assert!(spans.contains(&(Role::Keyword, "in".into())));
        assert!(spans.contains(&(Role::Keyword, "do".into())));
        assert!(spans.contains(&(Role::Keyword, "done".into())));
        assert!(spans.contains(&(Role::Command, "echo".into())));
        assert_eq!(spans.last(), Some(&(Role::Comment, "# x".into())));
        let spans = roles("if ls; then echo; fi");
        assert_eq!(spans[0], (Role::Keyword, "if".into()));
        assert!(spans.contains(&(Role::Keyword, "then".into())));
        assert!(spans.contains(&(Role::Keyword, "fi".into())));
    }

    #[test]
    fn assignments_and_redirections() {
        let spans = roles("X=1 ls > out 2>&1");
        assert_eq!(spans[0], (Role::Variable, "X=".into()));
        assert!(spans.contains(&(Role::Command, "ls".into())));
        assert!(spans.contains(&(Role::Operator, ">".into())));
        assert!(spans.contains(&(Role::Operator, "2>&".into())));
        // A redirection's file is not a command.
        let spans = roles("> nosuch ls");
        assert!(!spans.iter().any(|(r, _)| *r == Role::Unknown));
    }

    #[test]
    fn arrays_conditions_and_history_are_not_commands() {
        for line in [
            "a=(red green blue); echo ${a[1]}",
            "echo -A port=([http]=80)",
            "[[ dukovo-0.10 =~ ([a-z]+)-([0-9.]+) ]] && echo ok",
            "!!",
            "^two^three",
        ] {
            let spans = roles(line);
            assert!(!spans.iter().any(|(r, _)| *r == Role::Unknown), "{line}: {spans:?}");
            assert_eq!(joined(line), line);
        }
        assert!(roles("[[ -n x ]] && grp")[..].contains(&(Role::Unknown, "grp".into())));
    }
}
