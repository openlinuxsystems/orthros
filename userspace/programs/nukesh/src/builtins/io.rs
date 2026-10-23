// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Input and output: `echo`, `printf`, `read` and `mapfile`.
//!
//! `read` must not take more input than the line it returns, because the
//! commands after it read the rest: from a pipe or terminal it reads a
//! byte at a time, and from a file it reads ahead and moves the offset
//! back over what it did not use.

use std::time::{Duration, Instant};

use super::{flags, out, usage};
use crate::shell::{Res, Shell};
use crate::{sys, text, trap};

// Backslash escapes.

/// Which escapes a string has.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Escapes {
    /// `echo -e` and `printf %b`: `\c` ends the output, octal is `\0nnn`
    /// (and, for `%b`, `\nnn`).
    Echo,
    /// `printf`'s format: octal is `\nnn`.
    Format,
    /// `$'...'` and `${x@E}`: also `\'`, `\"`, `\?` and `\cX`.
    AnsiC,
}

/// Expands the backslash escapes of `s`; true as well if a `\c` ended it.
pub fn escapes(s: &str, mode: Escapes) -> (String, bool) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = chars[i + 1];
        i += 2;
        let simple = match e {
            'a' => Some('\x07'),
            'b' => Some('\x08'),
            'e' | 'E' => Some('\x1b'),
            'f' => Some('\x0c'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\x0b'),
            '\\' => Some('\\'),
            '\'' | '"' | '?' if mode != Escapes::Echo => Some(e),
            _ => None,
        };
        if let Some(c) = simple {
            out.push(c);
            continue;
        }
        match e {
            'c' if mode == Escapes::Echo => return (out, true),
            'c' if mode == Escapes::AnsiC && i < chars.len() => {
                // `\cX`: the control character of X.
                let x = chars[i];
                i += 1;
                out.push(char::from_u32(x.to_ascii_uppercase() as u32 ^ 0x40).unwrap_or(x));
            }
            '0'..='7' => {
                // Octal: up to three digits, after a leading 0 for echo.
                let mut digits = 3;
                let mut value = 0u32;
                let mut j = i - 1;
                if mode == Escapes::Echo && e == '0' {
                    j = i;
                } else if mode == Escapes::Echo {
                    digits = 3;
                }
                let start = j;
                while j < chars.len() && j - start < digits && chars[j].is_digit(8) {
                    value = value * 8 + chars[j].to_digit(8).unwrap_or(0);
                    j += 1;
                }
                i = j;
                out.push(text::byte_char((value & 0xff) as u8));
            }
            'x' => {
                let mut value = 0u32;
                let mut j = i;
                while j < chars.len() && j - i < 2 && chars[j].is_ascii_hexdigit() {
                    value = value * 16 + chars[j].to_digit(16).unwrap_or(0);
                    j += 1;
                }
                if j == i {
                    out.push_str("\\x");
                } else {
                    out.push(text::byte_char(value as u8));
                }
                i = j;
            }
            'u' | 'U' => {
                let max = if e == 'u' { 4 } else { 8 };
                let mut value = 0u32;
                let mut j = i;
                while j < chars.len() && j - i < max && chars[j].is_ascii_hexdigit() {
                    value = value.wrapping_mul(16) + chars[j].to_digit(16).unwrap_or(0);
                    j += 1;
                }
                if j == i {
                    out.push('\\');
                    out.push(e);
                } else if let Some(c) = char::from_u32(value) {
                    out.push(c);
                }
                i = j;
            }
            _ => {
                out.push('\\');
                out.push(e);
            }
        }
    }
    (out, false)
}

// echo.

pub fn echo(shell: &mut Shell, args: &[String]) -> Res {
    let xpg = shell.shopt("xpg_echo");
    let mut escape = xpg;
    let mut newline = true;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        let letters = arg.strip_prefix('-').filter(|l| !l.is_empty());
        // In POSIX mode with xpg_echo, as in dash, only -n is an option.
        let valid = letters.is_some_and(|l| {
            if xpg && shell.posix() {
                l.chars().all(|c| c == 'n')
            } else {
                l.chars().all(|c| "neE".contains(c))
            }
        });
        if !valid {
            break;
        }
        for c in letters.unwrap_or("").chars() {
            match c {
                'n' => newline = false,
                'e' => escape = true,
                _ => escape = false,
            }
        }
        i += 1;
    }
    let mut text = args[i..].join(" ");
    if escape {
        let (expanded, stopped) = escapes(&text, Escapes::Echo);
        text = expanded;
        if stopped {
            newline = false;
        }
    }
    if newline {
        text.push('\n');
    }
    Ok(i32::from(!out(shell, "echo", &text)))
}

// printf.

pub fn printf(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "printf", &args[1..], "v:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let Some(format) = flags.rest.first() else {
        return usage(shell, "printf", "");
    };
    let (output, status) = format_all(shell, format, &flags.rest[1..]);
    if let Some(var) = flags.value('v') {
        if let Err(message) = shell.assign(var, output, false) {
            shell.error(&format!("printf: {message}"));
            return Ok(1);
        }
        return Ok(status);
    }
    if !out(shell, "printf", &output) {
        return Ok(1);
    }
    Ok(status)
}

/// One conversion's flags, width and precision.
#[derive(Default)]
struct Spec {
    flags: String,
    width: Option<i64>,
    precision: Option<i64>,
}

impl Spec {
    /// The C format for this conversion.
    fn c_format(&self, conversion: &str) -> String {
        let mut f = String::from("%");
        f.push_str(&self.flags);
        if let Some(w) = self.width {
            f.push_str(&w.to_string());
        }
        if let Some(p) = self.precision {
            f.push('.');
            f.push_str(&p.max(0).to_string());
        }
        f.push_str(conversion);
        f
    }

    /// Pads `s` to the width, counting characters.
    fn pad(&self, s: &str) -> String {
        let s: String = match self.precision {
            Some(p) if p >= 0 => s.chars().take(p as usize).collect(),
            _ => s.to_string(),
        };
        let width = self.width.unwrap_or(0);
        let left = self.flags.contains('-') || width < 0;
        let width = width.unsigned_abs() as usize;
        let len = s.chars().count();
        if len >= width {
            return s;
        }
        let fill = " ".repeat(width - len);
        if left { s + &fill } else { fill + &s }
    }
}

/// Runs `format` over `args`, again while arguments are left.
fn format_all(shell: &mut Shell, format: &str, args: &[String]) -> (String, i32) {
    let format: Vec<char> = format.chars().collect();
    let mut output = String::new();
    let mut status = 0;
    let mut next = 0;
    loop {
        let before = next;
        let mut i = 0;
        while i < format.len() {
            let c = format[i];
            if c == '\\' {
                // An escape: up to the next conversion or backslash.
                let mut j = i + 1;
                if j < format.len() {
                    j += 1;
                    while j < format.len() && j - i < 10 && format[j] != '\\' && format[j] != '%' {
                        j += 1;
                    }
                }
                let piece: String = format[i..j].iter().collect();
                let (expanded, consumed) = escape_prefix(&piece);
                output.push_str(&expanded);
                i += consumed;
                continue;
            }
            if c != '%' {
                output.push(c);
                i += 1;
                continue;
            }
            i += 1;
            if format.get(i) == Some(&'%') {
                output.push('%');
                i += 1;
                continue;
            }
            let mut spec = Spec::default();
            while let Some(&f) = format.get(i) {
                if "-+ #0'".contains(f) {
                    if f != '\'' {
                        spec.flags.push(f);
                    }
                    i += 1;
                } else {
                    break;
                }
            }
            let number = |i: &mut usize, next: &mut usize, status: &mut i32| -> Option<i64> {
                if format.get(*i) == Some(&'*') {
                    *i += 1;
                    let arg = args.get(*next).map(String::as_str).unwrap_or("");
                    *next += 1;
                    let (n, ok) = parse_integer(arg);
                    if !ok {
                        shell.error(&format!("printf: {arg}: invalid number"));
                        *status = 1;
                    }
                    return Some(n);
                }
                let start = *i;
                while format.get(*i).is_some_and(|c| c.is_ascii_digit()) {
                    *i += 1;
                }
                (*i > start)
                    .then(|| format[start..*i].iter().collect::<String>().parse().unwrap_or(0))
            };
            spec.width = number(&mut i, &mut next, &mut status);
            if format.get(i) == Some(&'.') {
                i += 1;
                spec.precision = Some(number(&mut i, &mut next, &mut status).unwrap_or(0));
            }
            if let Some(w) = spec.width
                && w < 0
            {
                spec.flags.push('-');
                spec.width = Some(-w);
            }
            // Length modifiers mean nothing here.
            while format.get(i).is_some_and(|c| "hjlLtz".contains(*c)) {
                i += 1;
            }
            let Some(&conversion) = format.get(i) else {
                shell.error("printf: `%': missing format character");
                return (output, 1);
            };
            i += 1;
            let arg = args.get(next).cloned();
            match conversion {
                's' | 'b' | 'q' | 'Q' | 'c' | 'd' | 'i' | 'o' | 'u' | 'x' | 'X' | 'f' | 'F'
                | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                    next += 1;
                }
                '(' => {}
                _ => {
                    shell.error(&format!("printf: `{conversion}': invalid format character"));
                    return (output, 1);
                }
            }
            let arg_text = arg.clone().unwrap_or_default();
            match conversion {
                's' => output.push_str(&spec.pad(&arg_text)),
                'b' => {
                    let (expanded, stopped) = escapes(&arg_text, Escapes::Echo);
                    output.push_str(&spec.pad(&expanded));
                    if stopped {
                        return (output, status);
                    }
                }
                'q' => output.push_str(&spec.pad(&backslash_quote(&arg_text))),
                'Q' => {
                    let cut: String = match spec.precision {
                        Some(p) => arg_text.chars().take(p.max(0) as usize).collect(),
                        None => arg_text.clone(),
                    };
                    let quoted = backslash_quote(&cut);
                    spec.precision = None;
                    output.push_str(&spec.pad(&quoted));
                }
                'c' => {
                    let first: String = arg_text.chars().take(1).collect();
                    spec.precision = None;
                    output.push_str(&spec.pad(&first));
                }
                'd' | 'i' | 'o' | 'u' | 'x' | 'X' => {
                    let (n, ok) = parse_integer(&arg_text);
                    if !ok {
                        shell.error(&format!("printf: {arg_text}: invalid number"));
                        status = 1;
                    }
                    let conv = if conversion == 'i' {
                        "lld".to_string()
                    } else {
                        format!("ll{conversion}")
                    };
                    output.push_str(&c_format_int(&spec.c_format(&conv), n));
                }
                'f' | 'F' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                    let (x, ok) = parse_float(&arg_text);
                    if !ok {
                        shell.error(&format!("printf: {arg_text}: invalid number"));
                        status = 1;
                    }
                    output.push_str(&c_format_float(&spec.c_format(&conversion.to_string()), x));
                }
                '(' => {
                    // `%(format)T`: a time, given in seconds.
                    let mut time_format = String::new();
                    while let Some(&c) = format.get(i) {
                        i += 1;
                        if c == ')' {
                            break;
                        }
                        time_format.push(c);
                    }
                    if format.get(i) != Some(&'T') {
                        shell.error("printf: `(': invalid format character");
                        return (output, 1);
                    }
                    i += 1;
                    let arg = args.get(next).cloned();
                    next += 1;
                    let seconds = match arg.as_deref().map(parse_integer) {
                        None | Some((-1, _)) => sys::now().0,
                        Some((-2, _)) => sys::now().0 - shell.started.elapsed().as_secs() as i64,
                        Some((n, _)) => n,
                    };
                    let time_format =
                        if time_format.is_empty() { "%X".to_string() } else { time_format };
                    spec.precision = spec.precision.filter(|p| *p >= 0);
                    output.push_str(&spec.pad(&sys::strftime(&time_format, seconds)));
                }
                _ => {}
            }
        }
        // The format is used again while arguments are left, if it took
        // any.
        if next >= args.len() || next == before {
            break;
        }
    }
    (output, status)
}

/// Expands the one escape `piece` starts with: the text and how many
/// characters it took.
fn escape_prefix(piece: &str) -> (String, usize) {
    let chars: Vec<char> = piece.chars().collect();
    if chars.len() < 2 {
        return ("\\".into(), 1);
    }
    let e = chars[1];
    let take = |max: usize, radix: u32| -> usize {
        let mut n = 0;
        while 2 + n < chars.len() && n < max && chars[2 + n].is_digit(radix) {
            n += 1;
        }
        n
    };
    let len = match e {
        '0'..='7' => {
            let mut n = 1;
            while 1 + n < chars.len() && n < 3 && chars[1 + n].is_digit(8) {
                n += 1;
            }
            1 + n
        }
        'x' => 2 + take(2, 16),
        'u' => 2 + take(4, 16),
        'U' => 2 + take(8, 16),
        _ => 2,
    };
    let text: String = chars[..len].iter().collect();
    let (expanded, _) = escapes(&text, Escapes::Format);
    (expanded, len)
}

/// A number as printf takes it: a C integer constant, or `'c` for the
/// code of c. False as well if it was not all a number.
fn parse_integer(text: &str) -> (i64, bool) {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return (0, true);
    }
    if let Some(rest) = trimmed.strip_prefix('\'').or_else(|| trimmed.strip_prefix('"')) {
        return (
            rest.chars().next().map_or(0, |c| text::char_byte(c).map_or(c as i64, i64::from)),
            true,
        );
    }
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let (radix, body) =
        if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
            (16, hex)
        } else if digits.len() > 1 && digits.starts_with('0') {
            (8, &digits[1..])
        } else {
            (10, digits)
        };
    let end = body.find(|c: char| !c.is_digit(radix)).unwrap_or(body.len());
    let value =
        u64::from_str_radix(&body[..end], radix).unwrap_or(if end == 0 { 0 } else { u64::MAX });
    let value = if negative { (value as i64).wrapping_neg() } else { value as i64 };
    (value, end == body.len() && end > 0)
}

fn parse_float(text: &str) -> (f64, bool) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (0.0, true);
    }
    if trimmed.starts_with('\'') || trimmed.starts_with('"') {
        return (parse_integer(trimmed).0 as f64, true);
    }
    if let Ok(x) = trimmed.parse::<f64>() {
        return (x, true);
    }
    let (n, ok) = parse_integer(trimmed);
    if ok {
        return (n as f64, true);
    }
    // The longest number at the start.
    for end in (1..trimmed.len()).rev() {
        if let Some(prefix) = trimmed.get(..end)
            && let Ok(x) = prefix.parse::<f64>()
        {
            return (x, false);
        }
    }
    (0.0, false)
}

fn c_format_int(format: &str, value: i64) -> String {
    let mut buf = vec![0u8; 512];
    let c = text::cstring(format);
    // SAFETY: the format has one integer conversion (`ll`), which gets a
    // long long; snprintf writes at most buf.len() bytes.
    let n = unsafe {
        libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c.as_ptr(), value as libc::c_longlong)
    };
    buf.truncate((n.max(0) as usize).min(buf.len() - 1));
    text::decode(&buf)
}

fn c_format_float(format: &str, value: f64) -> String {
    let mut buf = vec![0u8; 512];
    let c = text::cstring(format);
    // SAFETY: the format has one floating-point conversion, which gets a
    // double; snprintf writes at most buf.len() bytes.
    let n = unsafe {
        libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c.as_ptr(), value as libc::c_double)
    };
    buf.truncate((n.max(0) as usize).min(buf.len() - 1));
    text::decode(&buf)
}

/// `s` quoted with backslashes, as `printf %q` does; `$'...'` if it has
/// control characters.
pub fn backslash_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().any(|c| c.is_control() || text::char_byte(c).is_some()) {
        return crate::expand::quote_always(s);
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        let special = matches!(
            c,
            ' ' | '\''
                | '"'
                | '\\'
                | '|'
                | '&'
                | ';'
                | '('
                | ')'
                | '<'
                | '>'
                | '!'
                | '{'
                | '}'
                | '*'
                | '['
                | ']'
                | '?'
                | '^'
                | '$'
                | '`'
                | ','
        ) || (i == 0 && matches!(c, '~' | '#'))
            || (c == '=' && i == 0);
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// Reading.

/// Why reading stopped early.
enum Stop {
    Eof,
    Timeout,
    /// A signal with a trap came.
    Signal(i32),
    Error(std::io::Error),
}

/// Input read a byte at a time, as far as anyone else can tell.
struct Input {
    fd: i32,
    regular: bool,
    buf: Vec<u8>,
    pos: usize,
    deadline: Option<Instant>,
}

impl Input {
    fn new(fd: i32, deadline: Option<Instant>) -> Input {
        Input { fd, regular: sys::is_regular(fd), buf: Vec::new(), pos: 0, deadline }
    }

    fn byte(&mut self, shell: &Shell) -> Result<u8, Stop> {
        if self.pos < self.buf.len() {
            self.pos += 1;
            return Ok(self.buf[self.pos - 1]);
        }
        loop {
            if let Some(deadline) = self.deadline {
                let left = deadline.saturating_duration_since(Instant::now());
                match sys::poll_in(self.fd, left.as_millis().min(i32::MAX as u128) as i32) {
                    Ok(true) => {}
                    Ok(false) => return Err(Stop::Timeout),
                    Err(error) if error.raw_os_error() == Some(libc::EINTR) => {
                        if let Some(signal) = trapped_signal(shell) {
                            return Err(Stop::Signal(signal));
                        }
                        continue;
                    }
                    Err(error) => return Err(Stop::Error(error)),
                }
            }
            let size = if self.regular { 4096 } else { 1 };
            self.buf.resize(size, 0);
            match sys::read(self.fd, &mut self.buf) {
                Ok(0) => {
                    self.buf.clear();
                    self.pos = 0;
                    return Err(Stop::Eof);
                }
                Ok(n) => {
                    self.buf.truncate(n);
                    self.pos = 1;
                    return Ok(self.buf[0]);
                }
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => {
                    self.buf.clear();
                    self.pos = 0;
                    if let Some(signal) = trapped_signal(shell) {
                        return Err(Stop::Signal(signal));
                    }
                }
                Err(error) => {
                    self.buf.clear();
                    self.pos = 0;
                    return Err(Stop::Error(error));
                }
            }
        }
    }

    /// Gives back what was read ahead.
    fn finish(&mut self) {
        let unread = self.buf.len().saturating_sub(self.pos);
        if unread > 0 && self.regular {
            let _ = sys::lseek(self.fd, -(unread as i64), libc::SEEK_CUR);
        }
        self.buf.clear();
        self.pos = 0;
    }
}

/// A signal that came and that a trap (or an interactive shell) wants
/// to stop a read for.
fn trapped_signal(shell: &Shell) -> Option<i32> {
    let signal = trap::first_pending()?;
    let wanted = shell.trap_command(signal as usize).is_some()
        || (shell.interactive && signal == libc::SIGINT);
    wanted.then_some(signal)
}

/// Reads a line from `fd`, without its newline: `None` at the end of the
/// input. For `select`.
pub fn read_line(fd: i32) -> Option<String> {
    let mut bytes = Vec::new();
    let mut got = false;
    let mut byte = [0u8; 1];
    loop {
        match sys::read(fd, &mut byte) {
            Ok(1) => {
                got = true;
                if byte[0] == b'\n' {
                    break;
                }
                bytes.push(byte[0]);
            }
            Ok(_) => break,
            Err(error) if error.raw_os_error() == Some(libc::EINTR) && !trap::pending() => {}
            Err(_) => break,
        }
    }
    got.then(|| text::decode(&bytes))
}

/// Decodes bytes read with a flag each (escaped by a backslash) to
/// characters with the flag.
fn decode_flagged(bytes: &[(u8, bool)]) -> Vec<(char, bool)> {
    let raw: Vec<u8> = bytes.iter().map(|(b, _)| *b).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let len = match raw[i] {
            0..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        match raw.get(i..i + len).and_then(|s| std::str::from_utf8(s).ok()) {
            Some(s) => {
                out.push((s.chars().next().unwrap_or('\0'), bytes[i].1));
                i += len;
            }
            None => {
                out.push((text::byte_char(raw[i]), bytes[i].1));
                i += 1;
            }
        }
    }
    out
}

/// Splits what `read` read into `count` fields by `ifs` (the last takes
/// the rest of the line), or into every field for `count` of `None`.
fn split_fields(line: &[(char, bool)], ifs: &str, count: Option<usize>) -> Vec<String> {
    let is_ws =
        |c: char, escaped: bool| !escaped && matches!(c, ' ' | '\t' | '\n') && ifs.contains(c);
    let is_delim = |c: char, escaped: bool| !escaped && ifs.contains(c);
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < line.len() && is_ws(line[pos].0, line[pos].1) {
        pos += 1;
    }
    loop {
        if count.is_some_and(|n| fields.len() + 1 == n) {
            // The last name: the rest, without trailing white space.
            let mut end = line.len();
            while end > pos && is_ws(line[end - 1].0, line[end - 1].1) {
                end -= 1;
            }
            fields.push(line[pos..end].iter().map(|(c, _)| *c).collect());
            break;
        }
        if pos >= line.len() {
            break;
        }
        let start = pos;
        while pos < line.len() && !is_delim(line[pos].0, line[pos].1) {
            pos += 1;
        }
        fields.push(line[start..pos].iter().map(|(c, _)| *c).collect());
        // The delimiter: white space around at most one other character.
        while pos < line.len() && is_ws(line[pos].0, line[pos].1) {
            pos += 1;
        }
        if pos < line.len()
            && is_delim(line[pos].0, line[pos].1)
            && !is_ws(line[pos].0, line[pos].1)
        {
            pos += 1;
            while pos < line.len() && is_ws(line[pos].0, line[pos].1) {
                pos += 1;
            }
            // A delimiter at the very end makes no empty last field.
            if pos >= line.len() && count.is_none() {
                break;
            }
            if pos >= line.len() {
                fields.push(String::new());
                break;
            }
        }
    }
    if let Some(n) = count {
        while fields.len() < n {
            fields.push(String::new());
        }
    }
    fields
}

/// Turns a terminal's echo or line editing off while reading, and back.
struct TermGuard {
    fd: i32,
    saved: Option<libc::termios>,
}

impl TermGuard {
    fn new(fd: i32, silent: bool, raw: bool) -> TermGuard {
        let mut guard = TermGuard { fd, saved: None };
        if !(silent || raw) || !sys::isatty(fd) {
            return guard;
        }
        if let Some(modes) = sys::tcgetattr(fd) {
            let mut changed = modes;
            if silent {
                changed.c_lflag &= !(libc::ECHO | libc::ECHONL);
            }
            if raw {
                changed.c_lflag &= !libc::ICANON;
                changed.c_cc[libc::VMIN] = 1;
                changed.c_cc[libc::VTIME] = 0;
            }
            sys::tcsetattr(fd, &changed);
            guard.saved = Some(modes);
        }
        guard
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        if let Some(modes) = &self.saved {
            sys::tcsetattr(self.fd, modes);
        }
    }
}

pub fn read(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "read", &args[1..], "a:d:ei:n:N:p:rst:u:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let fd = match flags.value('u') {
        Some(text) => match text.parse::<i32>() {
            Ok(fd) if fd >= 0 && sys::is_open(fd) => fd,
            _ => {
                shell.error(&format!("read: {text}: invalid file descriptor: Bad file descriptor"));
                return Ok(1);
            }
        },
        None => 0,
    };
    let names: Vec<String> = flags.rest.clone();
    for name in names.iter().chain(flags.value('a').map(str::to_string).iter()) {
        if !crate::parse::is_name(name.split('[').next().unwrap_or("")) {
            shell.error(&format!("read: `{name}': not a valid identifier"));
            return Ok(1);
        }
    }
    let raw_mode = flags.has('r');
    let delimiter = match flags.value('d') {
        Some(d) => d.bytes().next().unwrap_or(0),
        None => b'\n',
    };
    let parse_count = |shell: &Shell, letter: char| -> Result<Option<usize>, i32> {
        match flags.value(letter) {
            None => Ok(None),
            Some(text) => match text.parse::<usize>() {
                Ok(n) => Ok(Some(n)),
                Err(_) => {
                    shell.error(&format!("read: {text}: invalid number"));
                    Err(1)
                }
            },
        }
    };
    let (limit, exact) = match (parse_count(shell, 'n'), parse_count(shell, 'N')) {
        (Err(status), _) | (_, Err(status)) => return Ok(status),
        (Ok(_), Ok(Some(n))) => (Some(n), true),
        (Ok(n), Ok(None)) => (n, false),
    };
    let timeout = match flags.value('t') {
        Some(text) => match text.parse::<f64>() {
            Ok(t) if t >= 0.0 => Some(t),
            _ => {
                shell.error(&format!("read: {text}: invalid timeout specification"));
                return Ok(1);
            }
        },
        None => shell
            .get("TMOUT")
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|t| *t > 0.0 && sys::isatty(fd)),
    };
    if timeout == Some(0.0) {
        // `read -t 0`: whether input is waiting, reading none.
        return Ok(i32::from(!sys::poll_in(fd, 0).unwrap_or(false)));
    }
    let tty = sys::isatty(fd);
    if let Some(prompt) = flags.value('p')
        && tty
    {
        let _ = sys::write_all(2, &text::encode(prompt));
    }

    // With -e at a terminal, the line editor reads the line.
    if flags.has('e') && tty && fd == 0 {
        let prompt = flags.value('p').unwrap_or("");
        let initial = flags.value('i').unwrap_or("");
        return match crate::editor::read_line(shell, prompt, initial) {
            Some(line) => {
                let chars: Vec<(char, bool)> = line.chars().map(|c| (c, false)).collect();
                assign_read(shell, &chars, &names, flags.value('a'), false)
            }
            None => Ok(1),
        };
    }

    let deadline = timeout.map(|t| Instant::now() + Duration::from_secs_f64(t));
    let _guard = TermGuard::new(fd, flags.has('s'), limit.is_some() || delimiter != b'\n');
    let mut input = Input::new(fd, deadline);
    let mut bytes: Vec<(u8, bool)> = Vec::new();
    let mut chars_read = 0usize;
    let mut stop = None;
    loop {
        if limit.is_some_and(|n| chars_read >= n) {
            break;
        }
        let byte = match input.byte(shell) {
            Ok(byte) => byte,
            Err(why) => {
                stop = Some(why);
                break;
            }
        };
        if byte == delimiter && !exact {
            break;
        }
        if byte == b'\\' && !raw_mode {
            match input.byte(shell) {
                Ok(b'\n') if !exact || delimiter == b'\n' => {
                    // A line continuation.
                    if tty && shell.interactive && delimiter == b'\n' {
                        let ps2 = shell.get("PS2").unwrap_or_default();
                        let _ = sys::write_all(2, &text::encode(&ps2));
                    }
                    continue;
                }
                Ok(next) => {
                    bytes.push((next, true));
                    // The rest of a character the backslash escapes.
                    let more = match next {
                        0xc0..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf7 => 3,
                        _ => 0,
                    };
                    for _ in 0..more {
                        match input.byte(shell) {
                            Ok(b) => bytes.push((b, true)),
                            Err(why) => {
                                stop = Some(why);
                                break;
                            }
                        }
                    }
                    chars_read += 1;
                    if stop.is_some() {
                        break;
                    }
                    continue;
                }
                Err(why) => {
                    stop = Some(why);
                    break;
                }
            }
        }
        bytes.push((byte, false));
        // Count characters, not bytes, for -n and -N.
        if byte & 0xc0 != 0x80 {
            chars_read += 1;
        }
        if limit.is_some_and(|n| chars_read >= n) {
            // Finish the character.
            let more = match byte {
                0xc0..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf7 => 3,
                _ => 0,
            };
            for _ in 0..more {
                if let Ok(b) = input.byte(shell) {
                    bytes.push((b, false));
                }
            }
            break;
        }
    }
    input.finish();
    drop(_guard);
    let line = decode_flagged(&bytes);
    let status = match &stop {
        None => 0,
        Some(Stop::Eof) => 1,
        Some(Stop::Timeout) => 128 + libc::SIGALRM,
        Some(Stop::Signal(signal)) => {
            return Ok(128 + signal);
        }
        Some(Stop::Error(error)) => {
            shell.error(&format!("read: read error: {}: {}", fd, sys::describe(error)));
            return Ok(1);
        }
    };
    let assigned = assign_read(shell, &line, &names, flags.value('a'), exact)?;
    Ok(if assigned != 0 { assigned } else { status })
}

/// Puts what `read` read into its variables.
fn assign_read(
    shell: &mut Shell,
    line: &[(char, bool)],
    names: &[String],
    array: Option<&str>,
    exact: bool,
) -> Res {
    let ifs = shell.get("IFS").unwrap_or_else(|| " \t\n".into());
    let result = if let Some(array) = array {
        let fields = if exact {
            vec![line.iter().map(|(c, _)| *c).collect()]
        } else {
            split_fields(line, &ifs, None)
        };
        shell.assign_array(array, fields.into_iter().map(|f| (None, f)).collect(), false)
    } else if names.is_empty() {
        shell.assign("REPLY", line.iter().map(|(c, _)| *c).collect(), false)
    } else if exact {
        let mut result = shell.assign(&names[0], line.iter().map(|(c, _)| *c).collect(), false);
        for name in &names[1..] {
            result = result.and(shell.assign(name, String::new(), false));
        }
        result
    } else {
        let fields = split_fields(line, &ifs, Some(names.len()));
        let mut result = Ok(());
        for (name, field) in names.iter().zip(fields) {
            if let Err(message) = shell.assign(name, field, false) {
                result = Err(message);
            }
        }
        result
    };
    match result {
        Ok(()) => Ok(0),
        Err(message) => {
            shell.error(&format!("read: {message}"));
            Ok(1)
        }
    }
}

pub fn mapfile(shell: &mut Shell, args: &[String]) -> Res {
    let name = args[0].clone();
    let flags = match flags(shell, &name, &args[1..], "d:n:O:s:tu:C:c:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let number = |shell: &Shell, letter: char, default: i64| -> Result<i64, i32> {
        match flags.value(letter) {
            None => Ok(default),
            Some(text) => text.parse::<i64>().map_err(|_| {
                shell.error(&format!("{name}: {text}: invalid number"));
                1
            }),
        }
    };
    let (count, origin, skip, quantum) = match (
        number(shell, 'n', 0),
        number(shell, 'O', 0),
        number(shell, 's', 0),
        number(shell, 'c', 5000),
    ) {
        (Ok(n), Ok(o), Ok(s), Ok(c)) => (n, o, s, c.max(1)),
        _ => return Ok(1),
    };
    let fd = match flags.value('u') {
        Some(text) => match text.parse::<i32>() {
            Ok(fd) if sys::is_open(fd) => fd,
            _ => {
                shell.error(&format!("{name}: {text}: invalid file descriptor specification"));
                return Ok(1);
            }
        },
        None => 0,
    };
    let array = flags.rest.first().cloned().unwrap_or_else(|| "MAPFILE".into());
    if !crate::parse::is_name(&array) {
        shell.error(&format!("{name}: `{array}': not a valid identifier"));
        return Ok(1);
    }
    let delimiter = match flags.value('d') {
        Some(d) => d.bytes().next().unwrap_or(0),
        None => b'\n',
    };
    let strip = flags.has('t');
    let callback = flags.value('C').map(str::to_string);
    if !flags.has('O') {
        let _ = shell.assign_array(&array, Vec::new(), false);
    }
    let mut input = Input::new(fd, None);
    let mut line = Vec::new();
    let mut read = 0i64;
    let mut index = origin;
    let mut skipped = 0i64;
    loop {
        if count > 0 && read >= count {
            break;
        }
        let byte = match input.byte(shell) {
            Ok(byte) => Some(byte),
            Err(Stop::Eof) => None,
            Err(Stop::Signal(signal)) => {
                input.finish();
                return Ok(128 + signal);
            }
            Err(_) => None,
        };
        let end = byte.is_none_or(|b| b == delimiter);
        if let Some(b) = byte
            && (b != delimiter || !strip)
        {
            line.push(b);
        }
        if !end {
            continue;
        }
        if byte.is_none() && line.is_empty() {
            break;
        }
        let text_line = text::decode(&std::mem::take(&mut line));
        if skipped < skip {
            skipped += 1;
        } else {
            if let Some(callback) = &callback
                && (index - origin) % quantum == 0
                && index > origin
            {
                let command = format!("{callback} {index} {}", crate::expand::quote(&text_line));
                shell.eval_text(&command, &name)?;
            }
            if let Err(message) = shell.assign_element(&array, &index.to_string(), text_line, false)
            {
                shell.error(&format!("{name}: {message}"));
                input.finish();
                return Ok(1);
            }
            index += 1;
            read += 1;
        }
        if byte.is_none() {
            break;
        }
    }
    input.finish();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Vec<(char, bool)> {
        s.chars().map(|c| (c, false)).collect()
    }

    #[test]
    fn read_splits_as_posix_says() {
        let ifs = " \t\n";
        assert_eq!(split_fields(&plain("  a b  c  "), ifs, Some(2)), ["a", "b  c"]);
        assert_eq!(split_fields(&plain("a"), ifs, Some(3)), ["a", "", ""]);
        assert_eq!(split_fields(&plain("a:b::c"), ":", None), ["a", "b", "", "c"]);
        assert_eq!(split_fields(&plain("a:b:"), ":", None), ["a", "b"]);
        assert_eq!(split_fields(&plain(" a : b "), " :", None), ["a", "b"]);
    }

    #[test]
    fn escapes_expand() {
        assert_eq!(escapes("a\\tb\\n", Escapes::Echo).0, "a\tb\n");
        assert_eq!(escapes("x\\cy", Escapes::Echo), ("x".to_string(), true));
        assert_eq!(escapes("\\0101", Escapes::Echo).0, "A");
        assert_eq!(escapes("\\101", Escapes::Format).0, "A");
        assert_eq!(escapes("\\x41\\u00e9", Escapes::AnsiC).0, "Aé");
    }

    #[test]
    fn printf_numbers_parse_as_c_does() {
        assert_eq!(parse_integer("0x1f"), (31, true));
        assert_eq!(parse_integer("010"), (8, true));
        assert_eq!(parse_integer("'A"), (65, true));
        assert_eq!(parse_integer("12abc"), (12, false));
        assert_eq!(parse_integer("-5"), (-5, true));
    }

    #[test]
    fn quoting_for_printf_q() {
        assert_eq!(backslash_quote("a b"), "a\\ b");
        assert_eq!(backslash_quote(""), "''");
        assert_eq!(backslash_quote("it's"), "it\\'s");
        assert_eq!(backslash_quote("a\nb"), "$'a\\nb'");
    }
}
