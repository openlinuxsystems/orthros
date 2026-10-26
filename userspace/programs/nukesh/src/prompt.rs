// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Prompts: the backslash escapes of `PS1` and its kin (`\u`, `\w`, `\$`
//! and the rest, as bash has them), then, with `shopt -s promptvars`,
//! parameter expansion, command substitution and arithmetic.

use std::rc::Rc;

use crate::builtins::{Escapes, escapes};
use crate::shell::Shell;
use crate::{sys, text};

/// `${x@E}`: the backslash escapes of `$'...'` expanded.
pub fn decode_escapes(s: &str) -> String {
    escapes(s, Escapes::AnsiC).0
}

/// A prompt string expanded, ready to show.
pub fn expand(shell: &mut Shell, prompt: &str) -> String {
    let promptvars = shell.shopt("promptvars");
    let decoded = decode_prompt(shell, prompt, promptvars);
    if !promptvars {
        return decoded;
    }
    let saved = shell.opts.xtrace;
    shell.opts.xtrace = false;
    let saved_status = shell.status;
    let result =
        match crate::parse::string_word(&decoded, Rc::from("prompt"), shell.parse_options()) {
            Ok(word) => shell.expand_quoted(&word).unwrap_or_else(|_| decoded.clone()),
            Err(_) => decoded.clone(),
        };
    shell.status = saved_status;
    shell.opts.xtrace = saved;
    result
}

/// What an escape stands for, protected from the expansions after it.
fn protect(s: &str, promptvars: bool, out: &mut String) {
    for c in s.chars() {
        if promptvars && matches!(c, '$' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
}

fn decode_prompt(shell: &Shell, prompt: &str, promptvars: bool) -> String {
    let chars: Vec<char> = prompt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let (now, _) = sys::now();
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = chars[i + 1];
        i += 2;
        let value = match e {
            'a' => "\x07".to_string(),
            'e' => "\x1b".to_string(),
            'n' => "\n".to_string(),
            'r' => "\r".to_string(),
            '\\' => "\\".to_string(),
            'd' => sys::strftime("%a %b %d", now),
            't' => sys::strftime("%H:%M:%S", now),
            'T' => sys::strftime("%I:%M:%S", now),
            '@' => sys::strftime("%I:%M %p", now),
            'A' => sys::strftime("%H:%M", now),
            'D' => {
                // `\D{format}`.
                if chars.get(i) == Some(&'{') {
                    let close = chars[i..].iter().position(|c| *c == '}').map(|p| p + i);
                    match close {
                        Some(close) => {
                            let format: String = chars[i + 1..close].iter().collect();
                            i = close + 1;
                            sys::strftime(if format.is_empty() { "%X" } else { &format }, now)
                        }
                        None => "\\D".to_string(),
                    }
                } else {
                    "\\D".to_string()
                }
            }
            'h' => sys::hostname().split('.').next().unwrap_or("").to_string(),
            'H' => sys::hostname(),
            'j' => shell.jobs.list.len().to_string(),
            'l' => tty_name(),
            's' => {
                let name = shell.arg0.rsplit('/').next().unwrap_or(&shell.arg0);
                name.trim_start_matches('-').to_string()
            }
            'u' => sys::user_name().unwrap_or_else(|| sys::geteuid().to_string()),
            'v' => {
                let version = env!("CARGO_PKG_VERSION");
                version.splitn(3, '.').take(2).collect::<Vec<_>>().join(".")
            }
            'V' => env!("CARGO_PKG_VERSION").to_string(),
            'w' => short_pwd(shell, false),
            'W' => short_pwd(shell, true),
            '!' => shell.history.count().to_string(),
            '#' => shell.command_number.to_string(),
            '$' => if sys::geteuid() == 0 { "#" } else { "$" }.to_string(),
            '[' | ']' => String::new(),
            '0'..='7' => {
                let mut value = e.to_digit(8).unwrap_or(0);
                let mut n = 1;
                while n < 3 && chars.get(i).is_some_and(|c| c.is_digit(8)) {
                    value = value * 8 + chars[i].to_digit(8).unwrap_or(0);
                    i += 1;
                    n += 1;
                }
                text::byte_char((value & 0xff) as u8).to_string()
            }
            other => {
                out.push('\\');
                out.push(other);
                continue;
            }
        };
        protect(&value, promptvars, &mut out);
    }
    out
}

/// The working directory with the home directory as `~`; with `base`,
/// only its last part. `PROMPT_DIRTRIM` keeps that many trailing parts.
fn short_pwd(shell: &Shell, base: bool) -> String {
    let pwd = shell.get("PWD").unwrap_or_default();
    let home = shell.get("HOME").unwrap_or_default();
    let tilde =
        if !home.is_empty() && home != "/" && (pwd == home || pwd.starts_with(&format!("{home}/")))
        {
            format!("~{}", &pwd[home.len()..])
        } else {
            pwd.clone()
        };
    if base {
        if tilde == "~" || pwd == "/" {
            return tilde
                .chars()
                .next()
                .map_or_else(String::new, |c| if c == '~' { "~".into() } else { "/".into() });
        }
        return pwd.rsplit('/').next().unwrap_or("").to_string();
    }
    let trim = shell.get("PROMPT_DIRTRIM").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if trim > 0 {
        let parts: Vec<&str> = tilde.split('/').collect();
        if parts.len() > trim + 1 {
            let head = if tilde.starts_with('~') { "~/" } else { "" };
            return format!("{head}.../{}", parts[parts.len() - trim..].join("/"));
        }
    }
    tilde
}

/// The terminal's name without its directory, for `\l`.
fn tty_name() -> String {
    // SAFETY: ttyname returns a static string or null.
    let name = unsafe { libc::ttyname(0) };
    if name.is_null() {
        return "tty".into();
    }
    // SAFETY: a non-null result is a terminated string.
    let full = text::decode(unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes());
    full.rsplit('/').next().unwrap_or("tty").to_string()
}
