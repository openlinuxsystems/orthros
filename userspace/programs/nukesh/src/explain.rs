// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! "Explain this command" (ADR 0268): Alt+E at the prompt, and the
//! `explain` builtin, say what a command line does, part by part, with a
//! bold warning first for what cannot be taken back: deleting,
//! overwriting, changing permissions, running as the administrator, or
//! sending data off this computer.
//!
//! The plain answer comes from what is on this computer: each program's
//! line in its manual page, each option's line in its completion file,
//! and the builtins' own summaries. When Nebix, the local assistant, is
//! running, it is asked as well, over its socket and nowhere else; until
//! then the plain answer is the whole answer.
//!
//! Nothing is ever run. Secrets in the line (`--password X`, `TOKEN=...`,
//! `https://user:pass@host`) are masked before anything is shown or
//! asked, and explanations are not kept in the history.

use std::io::{BufRead, Read, Write};

/// Where the manual pages are.
const MAN_DIR: &str = "/system/resources/man";

/// A line of an explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// A warning, shown first and in bold.
    pub warning: bool,
    pub text: String,
}

/// What an explanation is made from.
pub trait Sources {
    /// What a program or builtin is, in a line.
    fn summary(&mut self, command: &str) -> Option<String>;
    /// What `option` of `command` does, and whether it takes an argument.
    fn option(&mut self, command: &str, option: &str) -> Option<(String, bool)>;
}

/// A word or an operator of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    /// `|`, `||`, `&&`, `;`, `&`.
    Op(String),
    /// `>`, `>>`, `<`, `2>`, `&>`, ... and what it names.
    Redirect(String, String),
}

/// The words names that hold secrets have.
const SECRET_NAMES: &[&str] =
    &["PASSWORD", "PASSWD", "PASS", "TOKEN", "SECRET", "KEY", "CREDENTIAL", "AUTH"];

/// Options whose argument is a secret.
const SECRET_OPTIONS: &[&str] = &[
    "--password",
    "--passwd",
    "--pass",
    "--token",
    "--secret",
    "--api-key",
    "--auth",
    "--access-token",
];

const MASK: &str = "•••";

/// Whether a variable's name looks like it holds a secret.
fn secret_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    SECRET_NAMES.iter().any(|s| upper.split('_').any(|part| part == *s) || upper.ends_with(s))
}

/// `line` with its secrets masked: values of secret-named variables and
/// options, and the password of a URL.
pub fn mask(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut hide_next = false;
    for (i, piece) in line.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if hide_next && !piece.is_empty() {
            out.push_str(MASK);
            hide_next = false;
            continue;
        }
        if SECRET_OPTIONS.contains(&piece) {
            hide_next = true;
            out.push_str(piece);
            continue;
        }
        if let Some((name, _)) = piece.split_once('=') {
            let bare = name.trim_start_matches('-');
            if (name.starts_with("--") && SECRET_OPTIONS.contains(&name))
                || (!name.starts_with('-') && crate::parse::is_name(bare) && secret_name(bare))
            {
                out.push_str(name);
                out.push('=');
                out.push_str(MASK);
                continue;
            }
        }
        out.push_str(&mask_url(piece));
    }
    out
}

/// Whether `line` holds a secret that [`mask`] would hide.
pub fn holds_secret(line: &str) -> bool {
    mask(line) != line
}

/// `scheme://user:pass@host` with the password masked.
fn mask_url(word: &str) -> String {
    let Some(start) = word.find("://") else { return word.to_string() };
    let rest = &word[start + 3..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..end].rfind('@') else { return word.to_string() };
    let Some(colon) = rest[..at].find(':') else { return word.to_string() };
    format!("{}{}:{MASK}{}", &word[..start + 3], &rest[..colon], &rest[at..])
}

/// The tokens of `line`, with quotes taken off words.
fn tokens(line: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut word = String::new();
    let mut in_word = false;
    let mut pending: Option<String> = None;
    let flush = |word: &mut String,
                 in_word: &mut bool,
                 pending: &mut Option<String>,
                 out: &mut Vec<Token>| {
        if *in_word {
            let w = std::mem::take(word);
            match pending.take() {
                Some(op) => out.push(Token::Redirect(op, w)),
                None => out.push(Token::Word(w)),
            }
            *in_word = false;
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' => flush(&mut word, &mut in_word, &mut pending, &mut out),
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '\\' if i + 1 < chars.len() => {
                in_word = true;
                word.push(chars[i + 1]);
                i += 1;
            }
            '|' | '&' | ';' | '>' | '<' => {
                // A digit just before a redirection is its descriptor.
                let fd = if matches!(c, '>' | '<')
                    && in_word
                    && word.chars().all(|d| d.is_ascii_digit())
                {
                    in_word = false;
                    std::mem::take(&mut word)
                } else {
                    flush(&mut word, &mut in_word, &mut pending, &mut out);
                    String::new()
                };
                let mut op = fd;
                op.push(c);
                let next = chars.get(i + 1).copied();
                match (c, next) {
                    ('|', Some('|')) | ('&' | '>', Some('&' | '>')) => {
                        op.push(next.unwrap_or_default());
                        i += 1;
                    }
                    _ => {}
                }
                if op.contains(['>', '<']) {
                    // `2>&1` names a descriptor, not a file.
                    if op.ends_with('&') && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                        i += 1;
                        while chars.get(i).is_some_and(char::is_ascii_digit) {
                            i += 1;
                        }
                        i -= 1;
                    } else {
                        pending = Some(op);
                    }
                } else {
                    out.push(Token::Op(op));
                }
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    flush(&mut word, &mut in_word, &mut pending, &mut out);
    out
}

/// A simple command's words and redirections.
type Simple = (Vec<String>, Vec<(String, String)>);

/// What a joining operator does.
fn joint(op: &str) -> &'static str {
    match op {
        "|" => "and its output goes to:",
        "||" => "and if it fails:",
        "&&" => "and if it works:",
        "&" => "in the background, and then:",
        _ => "then:",
    }
}

/// The parts of the explanation of `line`, warnings first.
pub fn explain(line: &str, sources: &mut dyn Sources) -> Vec<Part> {
    let masked = mask(line);
    let mut warnings: Vec<String> = Vec::new();
    let mut plain: Vec<String> = Vec::new();
    let tokens = tokens(&masked);
    let mut commands: Vec<Simple> = vec![(Vec::new(), Vec::new())];
    let mut joints = Vec::new();
    for token in tokens {
        match token {
            Token::Word(w) => commands.last_mut().unwrap_or_else(|| unreachable!()).0.push(w),
            Token::Redirect(op, target) => {
                commands.last_mut().unwrap_or_else(|| unreachable!()).1.push((op, target));
            }
            Token::Op(op) => {
                joints.push(op);
                commands.push((Vec::new(), Vec::new()));
            }
        }
    }
    let piped_into_shell = commands.iter().enumerate().any(|(i, (words, _))| {
        i > 0
            && joints.get(i - 1).is_some_and(|j| j == "|")
            && command_name(words).is_some_and(|(n, _)| {
                matches!(n.as_str(), "sh" | "bash" | "nukesh" | "dash" | "zsh")
            })
            && commands[i - 1].0.first().is_some_and(|w| matches!(base(w), "curl" | "wget"))
    });
    if piped_into_shell {
        warnings.push("Runs a script from the internet without showing it to you first".into());
    }
    for (index, (words, redirects)) in commands.iter().enumerate() {
        if index > 0 {
            plain.push(joints[index - 1].as_str().pipe(joint).to_string());
        }
        explain_command(words, redirects, sources, &mut warnings, &mut plain);
    }
    let mut parts: Vec<Part> = Vec::new();
    for w in warnings {
        if !parts.iter().any(|p| p.text == w) {
            parts.push(Part { warning: true, text: w });
        }
    }
    parts.extend(plain.into_iter().map(|text| Part { warning: false, text }));
    parts
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// The program a simple command runs and where its arguments start,
/// past assignments and `elevate`.
fn command_name(words: &[String]) -> Option<(String, usize)> {
    let mut at = 0;
    while let Some(word) = words.get(at) {
        let assignment = word.split_once('=').is_some_and(|(name, _)| crate::parse::is_name(name));
        if assignment
            || matches!(
                word.as_str(),
                "elevate" | "sudo" | "doas" | "time" | "command" | "exec" | "nohup"
            )
        {
            at += 1;
            continue;
        }
        return Some((base(word).to_string(), at + 1));
    }
    None
}

fn explain_command(
    words: &[String],
    redirects: &[(String, String)],
    sources: &mut dyn Sources,
    warnings: &mut Vec<String>,
    plain: &mut Vec<String>,
) {
    for word in words {
        if let Some((name, _)) = word.split_once('=')
            && crate::parse::is_name(name)
        {
            plain.push(format!("{name}=…  sets {name} for this command"));
            continue;
        }
        if matches!(word.as_str(), "elevate" | "sudo" | "doas") {
            warnings
                .push("Runs as the administrator, who can change anything on this computer".into());
            plain.push(format!("{word}  runs what follows as the administrator"));
            continue;
        }
        break;
    }
    let Some((name, start)) = command_name(words) else { return };
    let summary =
        sources.summary(&name).unwrap_or_else(|| "not in the manual on this computer".into());
    plain.push(format!("{name}  {summary}"));
    let args = &words[start..];
    let mut options: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();
    let mut i = 0;
    let mut no_more = false;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if no_more || !arg.starts_with('-') || arg == "-" {
            operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            no_more = true;
            continue;
        }
        let (form, value) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        if let Some((about, takes)) = sources.option(&name, &form) {
            let value = value.or_else(|| {
                (takes && i < args.len()).then(|| {
                    i += 1;
                    args[i - 1].clone()
                })
            });
            plain.push(format!("  {}  {about}", show_option(&form, value.as_deref())));
            options.push(form);
            continue;
        }
        // A group of short options: -xzf archive.
        if !form.starts_with("--") && form.len() > 2 {
            let letters: Vec<char> = form[1..].chars().collect();
            let mut k = 0;
            while k < letters.len() {
                let single = format!("-{}", letters[k]);
                k += 1;
                let Some((about, takes)) = sources.option(&name, &single) else {
                    plain.push(format!("  {single}  not described"));
                    options.push(single);
                    continue;
                };
                let value = if takes && k < letters.len() {
                    let v: String = letters[k..].iter().collect();
                    k = letters.len();
                    Some(v)
                } else if takes && i < args.len() {
                    i += 1;
                    Some(args[i - 1].clone())
                } else {
                    None
                };
                plain.push(format!("  {}  {about}", show_option(&single, value.as_deref())));
                options.push(single);
            }
            continue;
        }
        plain.push(format!("  {form}  not described"));
        options.push(form);
    }
    if !operands.is_empty() {
        plain.push(format!("  on: {}", operands.join(" ")));
    }
    for (op, target) in redirects {
        let text = match op.trim_start_matches(char::is_numeric) {
            ">" | "&>" | ">&" if target != "/dev/null" => {
                warnings.push(format!("Replaces what is in {target}"));
                format!("{op} {target}  writes the output to {target}, replacing what is there")
            }
            ">" | "&>" | ">&" => format!("{op} {target}  throws the output away"),
            ">>" => format!("{op} {target}  adds the output to the end of {target}"),
            "<" => format!("{op} {target}  reads the input from {target}"),
            _ => format!("{op} {target}"),
        };
        plain.push(format!("  {text}"));
    }
    warn(&name, &options, args, warnings);
}

fn show_option(form: &str, value: Option<&str>) -> String {
    match value {
        Some(v) if form.starts_with("--") => format!("{form}={v}"),
        Some(v) => format!("{form} {v}"),
        None => form.to_string(),
    }
}

/// The warnings a program and its options earn.
fn warn(name: &str, options: &[String], args: &[String], warnings: &mut Vec<String>) {
    let has = |o: &[&str]| options.iter().any(|x| o.contains(&x.as_str()));
    let recursive = has(&["-r", "-R", "--recursive"]);
    let remote = args.iter().any(|a| !a.starts_with('-') && a.contains(':') && !a.starts_with('/'));
    let text = match name {
        "rm" if recursive => {
            "Deletes files and whole folders for good: they do not go to the Trash"
        }
        "rm" | "unlink" => "Deletes files for good: they do not go to the Trash",
        "rmdir" => "Deletes folders",
        "shred" => "Destroys files so they cannot be got back",
        "truncate" => "Cuts files short, losing what is past the new size",
        "dd" if args.iter().any(|a| a.starts_with("of=")) => {
            "Writes straight over what of= names, a file or a whole disk"
        }
        "wipefs" | "fdisk" | "sfdisk" | "parted" | "gdisk" | "blkdiscard" => {
            "Changes or erases a disk's partitions"
        }
        n if n.starts_with("mkfs") => "Formats a disk or partition, erasing everything on it",
        "chmod" if recursive => "Changes who may use every file and folder under the ones named",
        "chown" | "chgrp" if recursive => {
            "Changes the owner of every file and folder under the ones named"
        }
        "chmod" => "Changes who may read, change or run the files named",
        "chown" | "chgrp" => "Changes who owns the files named",
        "mv" if !has(&["-i", "-n", "--no-clobber", "--interactive"]) => {
            "Replaces a file of the same name where it moves to, without asking"
        }
        "cp" if has(&["-f", "--force"]) => "Replaces files where it copies to, without asking",
        "scp" | "sftp" | "ftp" => "Sends data off this computer",
        "rsync" if remote => "Sends data off this computer",
        "rsync" if has(&["--delete"]) => {
            "Deletes files in the destination that are not in the source"
        }
        "curl"
            if has(&["-T", "--upload-file", "-d", "--data", "-F", "--form", "--data-binary"]) =>
        {
            "Sends data off this computer"
        }
        "nc" | "ncat" | "netcat" => "Sends data off this computer",
        "git" => match args.first().map(String::as_str) {
            Some("push")
                if has(&["-f", "--force"]) || args.iter().any(|a| a.starts_with("--force")) =>
            {
                "Rewrites the shared branch's history, which can lose others' work"
            }
            Some("push") => "Sends commits off this computer",
            Some("reset") if args.iter().any(|a| a == "--hard") => {
                "Throws away changes not committed"
            }
            Some("clean")
                if has(&["-f", "--force"])
                    || args
                        .iter()
                        .any(|a| a.starts_with("-f") || a.contains('f') && a.starts_with('-')) =>
            {
                "Deletes files git does not track"
            }
            Some("checkout" | "restore") if args.iter().any(|a| a == "." || a == "--") => {
                "Throws away changes not committed"
            }
            _ => return,
        },
        "kill" | "killall" | "pkill" => "Stops programs, which may lose what they have not saved",
        "reboot" | "poweroff" | "shutdown" | "halt" => "Turns the computer off or restarts it",
        _ => return,
    };
    warnings.push(text.to_string());
}

/// The summary line of `name`'s manual page, from `.Nd` (mdoc) or the
/// line after `.SH NAME` (man).
pub fn manual_summary(dir: &str, name: &str) -> Option<String> {
    for section in ["1", "8", "6", "7", "5"] {
        let path = format!("{dir}/man{section}/{name}.{section}");
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let mut lines = std::io::BufReader::new(file).lines().map_while(Result::ok).take(400);
        while let Some(line) = lines.next() {
            if let Some(about) = line.strip_prefix(".Nd ") {
                return Some(tidy(about));
            }
            // `.Sh NAME` (mdoc) is followed by `.Nd`; only man's `.SH`.
            if matches!(line.trim(), ".SH NAME" | ".SH \"NAME\"") {
                let next = lines.find(|l| !l.starts_with(".") || l.trim() == ".")?;
                let next = if next.trim() == "." { lines.next()? } else { next };
                // `\-` in man pages, a plain ` - ` in scdoc's.
                return next
                    .split_once("\\-")
                    .or_else(|| next.split_once(" - "))
                    .map(|(_, about)| tidy(about));
            }
        }
    }
    None
}

/// Roff text as plain text, starting with a capital and without a full
/// stop.
fn tidy(text: &str) -> String {
    let text = text
        .replace("\\*(Aq", "'")
        .replace("\\(aq", "'")
        .replace("\\-", "-")
        .replace("\\fB", "")
        .replace("\\fI", "")
        .replace("\\fR", "")
        .replace("\\fP", "")
        .replace("\\&", "")
        .replace('"', "");
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// What the meaning of an exit status usually is.
pub fn status_meaning(status: i32) -> String {
    match status {
        0 => "it worked".into(),
        1 => "it failed; the program's message above says why".into(),
        2 => "it was used wrongly, often a wrong option or a missing argument".into(),
        126 => "the file is there but cannot be run: it is not marked executable, or is a folder"
            .into(),
        127 => "there is no such command: a typo, or it is not installed".into(),
        130 => "it was stopped with Ctrl+C".into(),
        s if s > 128 && s < 160 => format!(
            "it was stopped by signal {}",
            crate::trap::SIGNALS
                .iter()
                .find(|(_, n)| *n == s - 128)
                .map_or_else(|| (s - 128).to_string(), |(name, _)| format!("SIG{name}"))
        ),
        _ => "it failed; the program's manual page says what this number means".into(),
    }
}

/// The shell's own sources: builtins, completion files and manual pages.
pub struct ShellSources<'a>(pub &'a mut crate::shell::Shell);

impl Sources for ShellSources<'_> {
    fn summary(&mut self, command: &str) -> Option<String> {
        if let Some(builtin) = crate::builtins::lookup(command) {
            let about = builtin.summary.trim_end_matches('.');
            return Some(format!("{about} (a Nukesh builtin)"));
        }
        let dir = self.0.get("NUKESH_MAN_DIR").unwrap_or_else(|| MAN_DIR.into());
        if let Some(alias) = self.0.aliases.get(command).cloned() {
            let first = alias.split_whitespace().next().unwrap_or_default();
            let about = (first != command)
                .then(|| self.summary(first))
                .flatten()
                .or_else(|| manual_summary(&dir, first));
            return Some(match about {
                Some(about) => format!("{about} (as the alias {command}='{alias}')"),
                None => format!("the alias {command}='{alias}'"),
            });
        }
        manual_summary(&dir, command)
    }

    fn option(&mut self, command: &str, option: &str) -> Option<(String, bool)> {
        let file = self.0.completion_file(command)?;
        let found = file.options.iter().find(|o| o.forms.iter().any(|f| f == option))?;
        let about = if found.description.is_empty() {
            "not described".into()
        } else {
            found.description.clone()
        };
        Some((about, found.argument.is_some()))
    }
}

/// The explanation as the terminal shows it.
pub fn render(parts: &[Part], nebix: Option<&str>, paint: bool) -> String {
    let mut out = String::new();
    for part in parts {
        if part.warning {
            // The console's font has no ⚠.
            let mark = if std::env::var("TERM").is_ok_and(|t| t == "linux") { "!" } else { "⚠" };
            let text = format!("{mark} {}", part.text);
            out.push_str(&if paint {
                nu_ansi_term::Style::new().bold().paint(text).to_string()
            } else {
                text
            });
        } else {
            out.push_str(&part.text);
        }
        out.push('\n');
    }
    if let Some(answer) = nebix {
        out.push_str(&if paint {
            nu_ansi_term::Style::new().bold().paint("Nebix:").to_string()
        } else {
            "Nebix:".into()
        });
        out.push('\n');
        out.push_str(answer);
        out.push('\n');
    } else if !parts.is_empty() {
        let note = "From the manual on this computer.";
        out.push_str(&if paint {
            nu_ansi_term::Style::new().dimmed().paint(note).to_string()
        } else {
            note.into()
        });
        out.push('\n');
    }
    out
}

/// What Alt+E shows for `line`; with an empty line, why the last command
/// failed.
pub fn for_prompt(shell: &mut crate::shell::Shell, line: &str, paint: bool) -> String {
    let (line, status) = if line.trim().is_empty() {
        if shell.status == 0 || shell.last_command.is_empty() {
            return String::new();
        }
        (shell.last_command.clone(), Some(shell.status))
    } else {
        (line.to_string(), None)
    };
    let parts = explain(&line, &mut ShellSources(shell));
    let mut out = String::new();
    if let Some(status) = status {
        let head =
            format!("{} ended with status {status}: {}", mask(&line), status_meaning(status));
        out.push_str(&head);
        out.push('\n');
    }
    let nebix = crate::nebix::explain(&mask(&line), status);
    out.push_str(&render(&parts, nebix.as_deref(), paint));
    out
}

/// `explain [command ...]`: explains the command given, or each line of
/// the input.
pub fn builtin(shell: &mut crate::shell::Shell, args: &[String]) -> crate::shell::Res {
    let args = &args[1..];
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        println!("usage: explain [command ...]");
        return Ok(0);
    }
    let paint = crate::sys::isatty(1) && shell.get("NO_COLOR").is_none_or(|v| v.is_empty());
    let lines: Vec<String> = if args.is_empty() {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_err() {
            return Ok(1);
        }
        text.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect()
    } else {
        vec![args.join(" ")]
    };
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let parts = explain(line, &mut ShellSources(shell));
        let nebix = crate::nebix::explain(&mask(line), None);
        out.push_str(&render(&parts, nebix.as_deref(), paint));
    }
    let _ = std::io::stdout().write_all(out.as_bytes());
    let _ = std::io::stdout().flush();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl Sources for Fake {
        fn summary(&mut self, command: &str) -> Option<String> {
            match command {
                "tar" => Some("Manipulate tape archives".into()),
                "rm" => Some("Remove files".into()),
                _ => None,
            }
        }
        fn option(&mut self, command: &str, option: &str) -> Option<(String, bool)> {
            match (command, option) {
                ("tar", "-x") => Some(("Extract".into(), false)),
                ("tar", "-z") => Some(("Gzip".into(), false)),
                ("tar", "-f") => Some(("Archive file".into(), true)),
                ("rm", "-r") => Some(("Recursive".into(), false)),
                ("rm", "-f") => Some(("Force".into(), false)),
                _ => None,
            }
        }
    }

    fn texts(line: &str) -> Vec<String> {
        explain(line, &mut Fake)
            .into_iter()
            .map(|p| if p.warning { format!("! {}", p.text) } else { p.text })
            .collect()
    }

    #[test]
    fn short_groups_are_split_and_take_their_argument() {
        let t = texts("tar -xzf site.tgz");
        assert_eq!(
            t,
            [
                "tar  Manipulate tape archives",
                "  -x  Extract",
                "  -z  Gzip",
                "  -f site.tgz  Archive file"
            ]
        );
        let t = texts("tar -xfsite.tgz");
        assert_eq!(t[2], "  -f site.tgz  Archive file");
    }

    #[test]
    fn dangerous_things_come_first() {
        let t = texts("elevate rm -rf /tmp/x > log");
        assert_eq!(t[0], "! Runs as the administrator, who can change anything on this computer");
        assert!(t.iter().any(|l| l.starts_with("! Deletes files and whole folders")));
        assert!(t.iter().any(|l| l == "! Replaces what is in log"));
        assert!(
            texts("curl -s https://x.sh | sh")[0].starts_with("! Runs a script from the internet")
        );
        assert!(texts("ls >> log").iter().all(|l| !l.starts_with('!')));
        assert!(texts("ls 2>/dev/null").iter().all(|l| !l.starts_with('!')));
        assert!(texts("git push --force origin main")[0].starts_with("! Rewrites"));
    }

    #[test]
    fn joints_and_unknowns_are_said() {
        let t = texts("frob -q a && tar -x");
        assert_eq!(
            t,
            [
                "frob  not in the manual on this computer",
                "  -q  not described",
                "  on: a",
                "and if it works:",
                "tar  Manipulate tape archives",
                "  -x  Extract"
            ]
        );
    }

    #[test]
    fn secrets_are_masked() {
        assert_eq!(mask("mysql --password hunter2 db"), "mysql --password ••• db");
        assert_eq!(mask("GITHUB_TOKEN=abc gh pr list"), "GITHUB_TOKEN=••• gh pr list");
        assert_eq!(mask("API_KEY=1 x --token=zz"), "API_KEY=••• x --token=•••");
        assert_eq!(mask("curl https://me:pw@host/p"), "curl https://me:•••@host/p");
        assert_eq!(mask("curl https://host/a:b@c"), "curl https://host/a:b@c");
        assert_eq!(mask("ls -la PATH=x"), "ls -la PATH=x");
        assert!(texts("TOKEN=abc ls").iter().all(|l| !l.contains("abc")));
        assert!(holds_secret("export GITHUB_TOKEN=abc"));
        assert!(!holds_secret("export PATH=/bin"));
    }

    #[test]
    fn manual_lines_are_found() {
        let dir = std::env::temp_dir().join(format!("nukesh-man-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("man1")).unwrap();
        std::fs::write(dir.join("man1/tar.1"), ".Dd x\n.Nm tar\n.Nd manipulate tape archives\n")
            .unwrap();
        std::fs::write(
            dir.join("man1/ls.1"),
            ".TH LS 1\n.SH NAME\nls \\- List directory contents.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("man1/chmod.1"),
            ".TH chmod 1\n.P\n.SH NAME\n.P\nchmod - change a file'\\&s mode\n",
        )
        .unwrap();
        let d = dir.to_str().unwrap();
        assert_eq!(manual_summary(d, "tar").as_deref(), Some("Manipulate tape archives"));
        assert_eq!(manual_summary(d, "ls").as_deref(), Some("List directory contents"));
        assert_eq!(manual_summary(d, "chmod").as_deref(), Some("Change a file's mode"));
        assert_eq!(manual_summary(d, "nope"), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn statuses_mean_something() {
        assert!(status_meaning(127).contains("no such command"));
        assert!(status_meaning(130).contains("Ctrl+C"));
    }
}
