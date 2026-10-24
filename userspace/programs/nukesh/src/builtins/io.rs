// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Input and output: `echo`, `printf`, `read` and `mapfile`.
//!
//! `read` must not take more input than the line it returns, because the
//! commands after it read the rest: from a pipe or terminal it reads a
//! byte at a time, and from a file it reads ahead and moves the offset
//! back over what it did not use.

use std::time::{Duration, Instant};

use super::{flags, legal_number, out, usage};
use crate::shell::{Res, Shell};
use crate::{sys, text, trap};

// Backslash escapes.

/// Which escapes a string has.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Escapes {
    /// `echo -e`: `\c` ends the output, octal is `\0nnn` only (`\101`
    /// stays as it is).
    Echo,
    /// `printf %b`: as `echo -e`, but octal is `\nnn` as well.
    PrintfB,
    /// `printf`'s format: octal is `\nnn`.
    Format,
    /// `$'...'` and `${x@E}`: also `\'`, `\"`, `\?` and `\cX`.
    AnsiC,
}

/// Expands the backslash escapes of `s`; true as well if a `\c` ended it.
pub fn escapes(s: &str, mode: Escapes) -> (String, bool) {
    let echo = matches!(mode, Escapes::Echo | Escapes::PrintfB);
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
            '\'' | '"' | '?' if !echo => Some(e),
            _ => None,
        };
        if let Some(c) = simple {
            out.push(c);
            continue;
        }
        match e {
            'c' if echo => return (out, true),
            'c' if mode == Escapes::AnsiC && i < chars.len() => {
                // `\cX`: the control character of X.
                let x = chars[i];
                i += 1;
                out.push(char::from_u32(x.to_ascii_uppercase() as u32 ^ 0x40).unwrap_or(x));
            }
            '1'..='7' if mode == Escapes::Echo => {
                out.push('\\');
                out.push(e);
            }
            '0'..='7' => {
                // Octal: up to three digits, which for echo and %b come
                // after a leading 0 (`\0101` is A there, `\010` and 1
                // in a format).
                let start = if echo && e == '0' { i } else { i - 1 };
                let mut value = 0u32;
                let mut j = start;
                while j < chars.len() && j - start < 3 && chars[j].is_digit(8) {
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
    let var = flags.value('v');
    if let Some(var) = var
        && !crate::parse::is_reference(var)
    {
        shell.error(&format!("printf: `{var}': not a valid identifier"));
        return Ok(2);
    }
    let Some(format) = flags.rest.first() else {
        return usage(shell, "printf", "");
    };
    let (mut output, status) = format_all(shell, format, &flags.rest[1..], var.is_none());
    if let Some(var) = var {
        // A variable holds no NUL: one ends the value, as in C.
        if let Some(nul) = output.find('\0') {
            output.truncate(nul);
        }
        return Ok(if shell.assign_from("printf", var, output)? { status } else { 1 });
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
    /// Whether the precision was written in the format, as digits, and
    /// not taken from an argument (`*`) or left out after the `.`.
    precision_digits: bool,
    /// Whether it was taken from an argument.
    precision_arg: bool,
}

impl Spec {
    /// The C format for this conversion. A negative precision is as if
    /// none was given, as in C.
    fn c_format(&self, conversion: &str) -> String {
        let mut f = String::from("%");
        f.push_str(&self.flags);
        if let Some(w) = self.width {
            f.push_str(&w.to_string());
        }
        if let Some(p) = self.precision.filter(|p| *p >= 0) {
            f.push('.');
            f.push_str(&p.to_string());
        }
        f.push_str(conversion);
        f
    }

    /// `s` cut to the precision and padded to the width, both counted in
    /// bytes, as C's printf counts them: a character can be cut in two.
    fn pad(&self, s: &str) -> String {
        let precision = self.precision.and_then(|p| usize::try_from(p).ok());
        let width = self.width.unwrap_or(0);
        let left = self.flags.contains('-') || width < 0;
        let width = width.unsigned_abs() as usize;
        if precision.is_none_or(|p| p >= s.len()) && s.len() >= width {
            return s.to_string();
        }
        let mut bytes = text::encode(s);
        if let Some(p) = precision {
            bytes.truncate(p);
        }
        if bytes.len() < width {
            let fill = std::iter::repeat_n(b' ', width - bytes.len());
            if left {
                bytes.extend(fill);
            } else {
                bytes.splice(0..0, fill);
            }
        }
        text::decode(&bytes)
    }
}

/// Runs `format` over `args`, again while arguments are left: the text
/// and the status. With `direct`, each pass but the last is written as
/// it ends, as bash writes them, so that its errors come between them;
/// the text is then the last pass's, still to write.
fn format_all(shell: &mut Shell, format: &str, args: &[String], direct: bool) -> (String, i32) {
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
            // A width or precision, an int as in C: from the format, or
            // from the next argument for `*`.
            let number = |i: &mut usize, next: &mut usize, status: &mut i32| -> Option<i64> {
                if format.get(*i) == Some(&'*') {
                    *i += 1;
                    let arg = args.get(*next).map_or("", String::as_str);
                    *next += 1;
                    let (n, how) = parse_signed(arg);
                    let int = n.clamp(i32::MIN.into(), i32::MAX.into());
                    let how = if how == Number::Whole && int != n { Number::Range } else { how };
                    check_number(shell, arg, how, status);
                    return Some(int);
                }
                let start = *i;
                while format.get(*i).is_some_and(char::is_ascii_digit) {
                    *i += 1;
                }
                (*i > start).then(|| {
                    let digits: String = format[start..*i].iter().collect();
                    digits.parse::<i64>().map_or(i32::MAX.into(), |n| n.min(i32::MAX.into()))
                })
            };
            spec.width = number(&mut i, &mut next, &mut status);
            if format.get(i) == Some(&'.') {
                i += 1;
                spec.precision_arg = format.get(i) == Some(&'*');
                let precision = number(&mut i, &mut next, &mut status);
                spec.precision_digits = precision.is_some() && !spec.precision_arg;
                spec.precision = Some(precision.unwrap_or(0));
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
                    let (expanded, stopped) = escapes(&arg_text, Escapes::PrintfB);
                    output.push_str(&spec.pad(&expanded));
                    if stopped {
                        return (output, status);
                    }
                }
                'q' => output.push_str(&spec.pad(&backslash_quote(&arg_text))),
                'Q' => {
                    // A precision written in the format cuts the argument,
                    // which is then quoted; one from an argument does
                    // nothing, as in bash.
                    let quoted = if spec.precision_digits {
                        let mut bytes = text::encode(&arg_text);
                        bytes.truncate(spec.precision.unwrap_or(0).max(0) as usize);
                        backslash_quote(&text::decode(&bytes))
                    } else {
                        backslash_quote(&arg_text)
                    };
                    if spec.precision_digits || spec.precision_arg {
                        spec.precision = None;
                    }
                    output.push_str(&spec.pad(&quoted));
                }
                'c' => {
                    // The first byte, as C has it; NUL for nothing.
                    let first =
                        text::encode(&arg_text).first().map_or('\0', |&b| text::byte_char(b));
                    spec.precision = None;
                    output.push_str(&spec.pad(&first.to_string()));
                }
                'd' | 'i' => {
                    let (n, how) = parse_signed(&arg_text);
                    check_number(shell, &arg_text, how, &mut status);
                    output.push_str(&c_format_int(&spec.c_format("lld"), n));
                }
                'o' | 'u' | 'x' | 'X' => {
                    let (n, how) = parse_unsigned(&arg_text);
                    check_number(shell, &arg_text, how, &mut status);
                    // The same bits, which the conversion takes as unsigned.
                    output.push_str(&c_format_int(
                        &spec.c_format(&format!("ll{conversion}")),
                        n as i64,
                    ));
                }
                'f' | 'F' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                    let (x, how) = parse_float(&arg_text);
                    check_number(shell, &arg_text, how, &mut status);
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
                    let given = arg.map_or(-1, |arg| {
                        let (n, how) = parse_signed(&arg);
                        check_number(shell, &arg, how, &mut status);
                        n
                    });
                    let seconds = match given {
                        -1 => sys::now().0,
                        -2 => sys::now().0 - shell.started.elapsed().as_secs() as i64,
                        n => n,
                    };
                    let time_format =
                        if time_format.is_empty() { "%X".to_string() } else { time_format };
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
        if direct {
            if !out(shell, "printf", &output) {
                return (String::new(), 1);
            }
            output.clear();
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

/// How much of an argument was the number printf took from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Number {
    Whole,
    /// It went on past the number, which is taken as far as it went.
    Partial,
    /// All number, but past what the conversion holds, which takes the
    /// nearest value it does.
    Range,
}

/// Reports a number that was not all number, an error that makes the
/// status 1, or out of range, a warning, as bash does.
fn check_number(shell: &Shell, arg: &str, how: Number, status: &mut i32) {
    match how {
        Number::Whole => {}
        Number::Partial => {
            shell.error(&format!("printf: {arg}: invalid number"));
            *status = 1;
        }
        Number::Range => {
            let reason = sys::describe(&std::io::Error::from_raw_os_error(libc::ERANGE));
            shell.error(&format!("printf: warning: {arg}: {reason}"));
        }
    }
}

/// `'c` or `"c`: the code of c.
fn char_code(text: &str) -> Option<i64> {
    let rest = text.strip_prefix('\'').or_else(|| text.strip_prefix('"'))?;
    Some(rest.chars().next().map_or(0, |c| text::char_byte(c).map_or(c as i64, i64::from)))
}

/// A C integer constant as strtoimax and strtoumax read one, after
/// blanks: whether it is negative, its size (None past u64), and whether
/// it was all of `text`.
fn c_integer(text: &str) -> (bool, Option<u64>, Number) {
    if text.is_empty() {
        return (false, Some(0), Number::Whole);
    }
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
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
    let size = if end == 0 { Some(0) } else { u64::from_str_radix(&body[..end], radix).ok() };
    let how = if end == body.len() && end > 0 { Number::Whole } else { Number::Partial };
    (negative, size, how)
}

/// A number for `%d` and `%i`, as strtoimax reads it, or `'c`.
fn parse_signed(text: &str) -> (i64, Number) {
    if let Some(code) = char_code(text) {
        return (code, Number::Whole);
    }
    let (negative, size, how) = c_integer(text);
    match size {
        Some(n) if negative && n <= 1 << 63 => ((n as i64).wrapping_neg(), how),
        Some(n) if !negative && n <= i64::MAX as u64 => (n as i64, how),
        _ => (
            if negative { i64::MIN } else { i64::MAX },
            if how == Number::Whole { Number::Range } else { how },
        ),
    }
}

/// A number for `%o`, `%u`, `%x` and `%X`, as strtoumax reads it: `-n`
/// is 2^64 - n.
fn parse_unsigned(text: &str) -> (u64, Number) {
    if let Some(code) = char_code(text) {
        return (code as u64, Number::Whole);
    }
    match c_integer(text) {
        (negative, Some(n), how) => (if negative { n.wrapping_neg() } else { n }, how),
        (_, None, how) => (u64::MAX, if how == Number::Whole { Number::Range } else { how }),
    }
}

/// A number for the floating-point conversions, as strtod reads it (hex
/// too), or `'c`.
fn parse_float(text: &str) -> (f64, Number) {
    if let Some(code) = char_code(text) {
        return (code as f64, Number::Whole);
    }
    let c = text::cstring(text);
    let start = c.as_ptr();
    let mut end = start.cast_mut();
    // SAFETY: `c` is NUL-terminated, and strtod leaves `end` inside it;
    // errno is this thread's.
    let (x, range, used) = unsafe {
        *libc::__errno_location() = 0;
        let x = libc::strtod(start, &mut end);
        (x, *libc::__errno_location() == libc::ERANGE, end.offset_from(start))
    };
    let how = if used.unsigned_abs() < c.as_bytes().len() {
        Number::Partial
    } else if range {
        Number::Range
    } else {
        Number::Whole
    };
    (x, how)
}

/// What C's snprintf makes of a format with one conversion, through
/// `print`, which calls it with a buffer, its size and the format; as
/// long as it comes.
fn c_format(
    format: &str,
    print: impl Fn(*mut libc::c_char, usize, *const libc::c_char) -> i32,
) -> String {
    let format = text::cstring(format);
    let mut buf = vec![0u8; 128];
    loop {
        let Ok(n) = usize::try_from(print(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr()))
        else {
            return String::new();
        };
        if n < buf.len() {
            buf.truncate(n);
            return text::decode(&buf);
        }
        buf.resize(n + 1, 0);
    }
}

fn c_format_int(format: &str, value: i64) -> String {
    // SAFETY: the format has one integer conversion (`ll`), which gets a
    // long long; snprintf writes at most `size` bytes to `buf`.
    c_format(format, |buf, size, format| unsafe {
        libc::snprintf(buf, size, format, value as libc::c_longlong)
    })
}

fn c_format_float(format: &str, value: f64) -> String {
    // SAFETY: the format has one floating-point conversion, which gets a
    // double; snprintf writes at most `size` bytes to `buf`.
    c_format(format, |buf, size, format| unsafe {
        libc::snprintf(buf, size, format, value as libc::c_double)
    })
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
        ) || (i == 0 && matches!(c, '~' | '#'));
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
    let ws_at = |i: usize| i < line.len() && is_ws(line[i].0, line[i].1);
    let text = |from: usize, to: usize| line[from..to].iter().map(|(c, _)| *c).collect::<String>();
    // The word at `pos`: where it ends, and where the delimiter after it
    // ends, white space around at most one other character.
    let word = |pos: usize| -> (usize, usize) {
        let mut end = pos;
        while end < line.len() && !is_delim(line[end].0, line[end].1) {
            end += 1;
        }
        let mut next = end;
        while ws_at(next) {
            next += 1;
        }
        if next < line.len() && !ws_at(next) && is_delim(line[next].0, line[next].1) {
            next += 1;
            while ws_at(next) {
                next += 1;
            }
        }
        (end, next)
    };
    let mut fields = Vec::new();
    let mut pos = 0;
    while ws_at(pos) {
        pos += 1;
    }
    loop {
        if count.is_some_and(|n| fields.len() + 1 == n) {
            // The last name: the one word left, if it and its delimiter
            // are all that is left, else the rest without trailing white
            // space, as bash has it (`IFS=: read a b <<< 1:2:` gives b 2).
            let (mut end, next) = word(pos);
            if next < line.len() {
                end = line.len();
                while end > pos && ws_at(end - 1) {
                    end -= 1;
                }
            }
            fields.push(text(pos, end));
            break;
        }
        if pos >= line.len() {
            break;
        }
        let (end, next) = word(pos);
        fields.push(text(pos, end));
        pos = next;
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
    let fd = match flags.value('u').map(|text| descriptor(shell, "read", text)) {
        Some(Some(fd)) => fd,
        Some(None) => return Ok(1),
        None => 0,
    };
    let names: Vec<String> = flags.rest.clone();
    // The first name is checked before reading, the rest (and `-a`'s)
    // as they are set, as in bash.
    if let Some(name) = names.first()
        && !flags.has('a')
        && !crate::parse::is_reference(name)
    {
        shell.error(&format!("read: `{name}': not a valid identifier"));
        return Ok(1);
    }
    let raw_mode = flags.has('r');
    let delimiter = match flags.value('d') {
        Some(d) => d.bytes().next().unwrap_or(0),
        None => b'\n',
    };
    let parse_count = |shell: &Shell, letter: char| -> Result<Option<usize>, i32> {
        match flags.value(letter) {
            None => Ok(None),
            Some(text) => {
                match legal_number(text).filter(|n| i32::try_from(*n).is_ok_and(|n| n >= 0)) {
                    Some(n) => Ok(Some(n as usize)),
                    None => {
                        shell.error(&format!("read: {text}: invalid number"));
                        Err(1)
                    }
                }
            }
        }
    };
    let (limit, exact) = match (parse_count(shell, 'n'), parse_count(shell, 'N')) {
        (Err(status), _) | (_, Err(status)) => return Ok(status),
        (Ok(_), Ok(Some(n))) => (Some(n), true),
        (Ok(n), Ok(None)) => (n, false),
    };
    let timeout = match flags.value('t') {
        Some(text) => match parse_timeout(text) {
            Some(t) => Some(t),
            None => {
                shell.error(&format!("read: {text}: invalid timeout specification"));
                return Ok(1);
            }
        },
        None => shell
            .get("TMOUT")
            .and_then(|t| parse_timeout(&t))
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

    // A timeout too long for the clock is none.
    let deadline = timeout
        .and_then(|t| Duration::try_from_secs_f64(t).ok())
        .and_then(|t| Instant::now().checked_add(t));
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
        // NUL bytes are left out, as bash leaves them.
        if byte == 0 {
            continue;
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

/// The descriptor `-u` names, for `read` and `mapfile`: None, with
/// bash's error, if it is not a number or not open.
fn descriptor(shell: &Shell, builtin: &str, text: &str) -> Option<i32> {
    let Some(fd) = legal_number(text).and_then(|n| i32::try_from(n).ok()).filter(|n| *n >= 0)
    else {
        shell.error(&format!("{builtin}: {text}: invalid file descriptor specification"));
        return None;
    };
    if !sys::is_open(fd) {
        let reason = sys::describe(&std::io::Error::from_raw_os_error(libc::EBADF));
        shell.error(&format!("{builtin}: {fd}: invalid file descriptor: {reason}"));
        return None;
    }
    Some(fd)
}

/// A timeout as `read -t` and TMOUT take one: decimal seconds, with a
/// fraction and a sign, not negative. Nothing at all, or `.`, is 0.
fn parse_timeout(text: &str) -> Option<f64> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    if !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let t: f64 = format!("0{whole}.{fraction}0").parse().ok()?;
    (!negative || t == 0.0).then_some(t)
}

/// Puts what `read` read into its variables, in order: a name that is
/// not valid, or a variable that refuses its value, stops it there, with
/// status 1, as in bash.
fn assign_read(
    shell: &mut Shell,
    line: &[(char, bool)],
    names: &[String],
    array: Option<&str>,
    exact: bool,
) -> Res {
    let ifs = shell.get("IFS").unwrap_or_else(|| " \t\n".into());
    let whole = || line.iter().map(|(c, _)| *c).collect::<String>();
    if let Some(array) = array {
        if !crate::parse::is_name(array) {
            shell.error(&format!("read: `{array}': not a valid identifier"));
            return Ok(1);
        }
        let fields = if exact { vec![whole()] } else { split_fields(line, &ifs, None) };
        let elements = fields.into_iter().map(|f| (None, f)).collect();
        return Ok(if shell.assign_array_from("read", array, elements)? { 0 } else { 1 });
    }
    // With no name, REPLY gets the line as it is.
    let reply = ["REPLY".to_string()];
    let (names, values) = if names.is_empty() {
        (&reply[..], vec![whole()])
    } else if exact {
        let mut values = vec![String::new(); names.len()];
        values[0] = whole();
        (names, values)
    } else {
        (names, split_fields(line, &ifs, Some(names.len())))
    };
    for (name, value) in names.iter().zip(values) {
        if !crate::parse::is_reference(name) {
            shell.error(&format!("read: `{name}': not a valid identifier"));
            return Ok(1);
        }
        if !shell.assign_from("read", name, value)? {
            return Ok(1);
        }
    }
    Ok(0)
}

pub fn mapfile(shell: &mut Shell, args: &[String]) -> Res {
    let name = args[0].clone();
    let flags = match flags(shell, &name, &args[1..], "d:n:O:s:tu:C:c:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    // The numbers, checked in order, as bash does.
    let (mut count, mut origin, mut skip, mut quantum, mut fd) = (0, 0, 0, 5000, 0);
    for (letter, value) in &flags.on {
        let text = value.as_deref().unwrap_or("");
        let (least, what) = match letter {
            'n' | 's' => (0, "line count"),
            'O' => (0, "array origin"),
            'c' => (1, "callback quantum"),
            'u' => {
                let Some(given) = descriptor(shell, &name, text) else { return Ok(1) };
                fd = given;
                continue;
            }
            _ => continue,
        };
        let Some(n) = legal_number(text).filter(|n| *n >= least) else {
            shell.error(&format!("{name}: {text}: invalid {what}"));
            return Ok(1);
        };
        match letter {
            'n' => count = n,
            's' => skip = n,
            'O' => origin = n,
            _ => quantum = n,
        }
    }
    let array = flags.rest.first().cloned().unwrap_or_else(|| "MAPFILE".into());
    if !crate::parse::is_name(&array) {
        shell.error(&format!("{name}: `{array}': not a valid identifier"));
        return Ok(1);
    }
    if let Err(message) = shell.writable(&shell.resolve(&array)) {
        shell.error(&message);
        return Ok(1);
    }
    if shell.assoc(&array) {
        shell.error(&format!("{name}: {array}: not an indexed array"));
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
        // A NUL ends the line's value, as in bash.
        let mut bytes = std::mem::take(&mut line);
        if let Some(nul) = bytes.iter().position(|&b| b == 0) {
            bytes.truncate(nul);
        }
        let text_line = text::decode(&bytes);
        if skipped < skip {
            skipped += 1;
        } else {
            // After every `quantum` lines, before the last is stored, with
            // the line in single quotes. bash names the callback's text for
            // no file, "stdin", and goes on after a syntax error in it.
            if let Some(callback) = &callback
                && (index - origin + 1) % quantum == 0
            {
                let line = text_line.replace('\'', "'\\''");
                let command = format!("{callback} {index} '{line}'");
                shell.syntax_failed = false;
                let result = shell.eval_text(&command, &name, "stdin");
                if !std::mem::take(&mut shell.syntax_failed) {
                    result?;
                }
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
        assert_eq!(escapes("\\101|\\1", Escapes::Echo).0, "\\101|\\1");
        assert_eq!(escapes("\\101|\\0101", Escapes::PrintfB).0, "A|A");
        assert_eq!(escapes("\\0101", Escapes::Format).0, "\x081");
        assert_eq!(escapes("\\101", Escapes::Format).0, "A");
        assert_eq!(escapes("\\x41\\u00e9", Escapes::AnsiC).0, "Aé");
    }

    #[test]
    fn printf_numbers_parse_as_c_does() {
        assert_eq!(parse_signed("0x1f"), (31, Number::Whole));
        assert_eq!(parse_signed("010"), (8, Number::Whole));
        assert_eq!(parse_signed("'A"), (65, Number::Whole));
        assert_eq!(parse_signed("12abc"), (12, Number::Partial));
        assert_eq!(parse_signed("-5"), (-5, Number::Whole));
        assert_eq!(parse_signed("  "), (0, Number::Partial));
        assert_eq!(parse_signed(""), (0, Number::Whole));
        assert_eq!(parse_signed("-9223372036854775808"), (i64::MIN, Number::Whole));
        assert_eq!(parse_signed("9223372036854775808"), (i64::MAX, Number::Range));
        assert_eq!(parse_signed("99999999999999999999x"), (i64::MAX, Number::Partial));
        assert_eq!(parse_unsigned("-1"), (u64::MAX, Number::Whole));
        assert_eq!(parse_unsigned("18446744073709551616"), (u64::MAX, Number::Range));
        assert_eq!(parse_float("0x1.8p1"), (3.0, Number::Whole));
        assert_eq!(parse_float("1e"), (1.0, Number::Partial));
        assert_eq!(parse_float("1e5000").1, Number::Range);
    }

    #[test]
    fn printf_pads_in_bytes() {
        let spec = Spec { width: Some(3), ..Spec::default() };
        assert_eq!(spec.pad("é"), " é");
        let spec = Spec { precision: Some(1), ..Spec::default() };
        assert_eq!(spec.pad("é"), text::decode(&[0xc3]));
        let spec = Spec { precision: Some(-1), ..Spec::default() };
        assert_eq!(spec.pad("abc"), "abc");
        assert_eq!(c_format_int("%600d", 1).len(), 600);
    }

    #[test]
    fn quoting_for_printf_q() {
        assert_eq!(backslash_quote("a b"), "a\\ b");
        assert_eq!(backslash_quote(""), "''");
        assert_eq!(backslash_quote("it's"), "it\\'s");
        assert_eq!(backslash_quote("a\nb"), "$'a\\nb'");
    }
}
