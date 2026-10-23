// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! hexdump(1) and hd(1): files' bytes in the formats util-linux's
//! hexdump prints, given as format strings (`-e`, `-f`) or by the options
//! that name the common ones (`-C`, `-x`, ...). A format string is BSD
//! hexdump's: units of `[count/][bytes] "format"`, each a printf(3) format
//! applied `count` times to `bytes` bytes of a block, with hexdump's own
//! conversions (`%_a` and `%_A` for addresses, `%_c`, `%_p` and `%_u` for
//! characters). Every format string reads the same block, as long as the
//! longest one takes; a block like the one before it is shown as `*`,
//! unless `-v` is given. `hd` is `hexdump -C`.

use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail};

/// The formats the options name, as util-linux defines them.
const CANONICAL: [&str; 3] = [
    "\"%08.8_Ax\\n\"",
    "\"%08.8_ax  \" 8/1 \"%02x \" \"  \" 8/1 \"%02x \" ",
    "\"  |\" 16/1 \"%_p\" \"|\\n\"",
];
const DEFAULT: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 8/2 \"%04x \" \"\\n\""];
const OCTAL_BYTES: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 16/1 \"%03o \" \"\\n\""];
const CHARACTERS: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 16/1 \"%3_c \" \"\\n\""];
const DECIMAL: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 8/2 \"  %05u \" \"\\n\""];
const OCTAL: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 8/2 \" %06o \" \"\\n\""];
const HEX: [&str; 2] = ["\"%07.7_Ax\\n\"", "\"%07.7_ax \" 8/2 \"   %04x \" \"\\n\""];

/// The characters a conversion's flags, width and precision are made of.
const SPEC: &[u8] = b".#-+ 0123456789";

/// What a conversion prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// `%_a`: the address of the next byte (`%_A`: of the end).
    Address,
    /// `%d`, `%i`.
    Signed,
    /// `%o`, `%u`, `%x`, `%X`.
    Unsigned,
    /// `%e`, `%f`, `%g` and their capitals.
    Float,
    /// `%c`.
    Char,
    /// `%s`, of a precision's or a byte count's bytes.
    Str,
    /// `%_c`: a character, a C escape, or three octal digits.
    Escaped,
    /// `%_p`: a printable character, or `.`.
    Printable,
    /// `%_u`: a printable character, a control character's name, or hex.
    Named,
}

/// A conversion and the bytes it reads.
#[derive(Clone, Debug)]
struct Conversion {
    kind: Kind,
    /// Its flags, width and precision, for printf(3).
    spec: String,
    /// printf(3)'s conversion character, where the kind has one.
    letter: char,
    bytes: usize,
    /// The width it fills where the data has ended.
    width: usize,
}

/// Text, then a conversion if it has one.
#[derive(Clone, Debug)]
struct Part {
    text: Vec<u8>,
    conversion: Option<Conversion>,
    /// Whether the text's last character, a blank, is left out on the
    /// unit's last repetition.
    no_space: bool,
}

/// A format unit: its parts, repeated.
#[derive(Clone, Debug)]
struct Unit {
    reps: usize,
    /// Whether the count was given (otherwise the unit may be repeated to
    /// fill the block).
    counted: bool,
    /// The byte count given.
    bytes: Option<usize>,
    parts: Vec<Part>,
    /// Whether it prints the end address (`%_A`): it is printed once, at
    /// the end, and not for each block.
    end: bool,
}

impl Unit {
    /// The bytes one repetition reads.
    fn size(&self) -> usize {
        self.bytes.unwrap_or_else(|| {
            self.parts.iter().filter_map(|p| p.conversion.as_ref()).map(|c| c.bytes).sum()
        })
    }
}

/// A format string's units.
type Format = Vec<Unit>;

/// Replaces the C escapes in a format.
fn unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.iter();
    while let Some(&b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        out.push(match bytes.next() {
            None => b'\\',
            Some(b'a') => 7,
            Some(b'b') => 8,
            Some(b'f') => 12,
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(b't') => b'\t',
            Some(b'v') => 11,
            Some(&other) => other,
        });
    }
    out
}

/// Parses a format string into its units.
fn parse(text: &str) -> Result<Format, Fail> {
    let bad = || fail(format!("bad format {{{text}}}"));
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut units = Vec::new();
    let digits = |at: &mut usize| -> Option<usize> {
        let start = *at;
        while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        text[start..*at].parse().ok()
    };
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        let mut reps = 1;
        let mut counted = false;
        let mut count = None;
        if bytes[at].is_ascii_digit() {
            reps = digits(&mut at).ok_or_else(bad)?;
            counted = true;
            if !bytes.get(at).is_some_and(|b| b.is_ascii_whitespace() || *b == b'/') {
                return Err(bad());
            }
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
        }
        if bytes.get(at) == Some(&b'/') {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
        }
        if bytes.get(at).is_some_and(u8::is_ascii_digit) {
            count = Some(digits(&mut at).ok_or_else(bad)?);
            if !bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                return Err(bad());
            }
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
        }
        if bytes.get(at) != Some(&b'"') {
            return Err(bad());
        }
        let start = at + 1;
        let Some(length) = bytes[start..].iter().position(|&b| b == b'"') else {
            return Err(bad());
        };
        at = start + length + 1;
        let format = unescape(&bytes[start..start + length]);
        let (parts, end) = parts(&format, count)?;
        units.push(Unit { reps, counted, bytes: count, parts, end });
    }
    Ok(units)
}

/// Splits a unit's format into its parts, as BSD hexdump does: each
/// conversion with the text before it, then any text after the last.
fn parts(format: &[u8], count: Option<usize>) -> Result<(Vec<Part>, bool), Fail> {
    let mut parts = Vec::new();
    let mut end = false;
    let mut rest = format;
    let mut conversions = 0;
    while !rest.is_empty() {
        let Some(percent) = rest.iter().position(|&b| b == b'%') else {
            parts.push(Part { text: rest.to_vec(), conversion: None, no_space: false });
            break;
        };
        let text = rest[..percent].to_vec();
        let mut at = percent + 1;
        // With a byte count, a precision means nothing; without one, a
        // precision is what tells `%s` how much to read.
        let mut precision = None;
        if count.is_some() {
            while rest.get(at).is_some_and(|b| SPEC.contains(b)) {
                at += 1;
            }
        } else {
            while rest.get(at).is_some_and(|b| SPEC[1..].contains(b)) {
                at += 1;
            }
            if rest.get(at) == Some(&b'.') && rest.get(at + 1).is_some_and(u8::is_ascii_digit) {
                at += 1;
                let start = at;
                while rest.get(at).is_some_and(u8::is_ascii_digit) {
                    at += 1;
                }
                precision = std::str::from_utf8(&rest[start..at])
                    .ok()
                    .and_then(|p| p.parse::<usize>().ok());
            } else if rest.get(at) == Some(&b'.') {
                at += 1;
            }
        }
        let spec = String::from_utf8_lossy(&rest[percent + 1..at]).into_owned();
        // A huge width would be a huge allocation.
        if spec.split(|c: char| !c.is_ascii_digit()).any(|n| n.len() > 4) {
            return Err(fail(format!("bad format {{%{spec}}}")));
        }
        let letter = rest.get(at).copied().unwrap_or(0) as char;
        let bad_count =
            |name: &str| fail(format!("bad byte count for conversion character {name}"));
        let bad_conversion = |name: &str| fail(format!("bad conversion character %{name}"));
        let mut next = at + 1;
        let (kind, letter, bytes) = match letter {
            'c' => match count {
                None | Some(1) => (Kind::Char, 'c', 1),
                _ => return Err(bad_count("c")),
            },
            'd' | 'i' | 'o' | 'u' | 'x' | 'X' => {
                let kind = if matches!(letter, 'd' | 'i') { Kind::Signed } else { Kind::Unsigned };
                match count {
                    None | Some(4) => (kind, letter, 4),
                    Some(n @ (1 | 2 | 8)) => (kind, letter, n),
                    _ => return Err(bad_count(&letter.to_string())),
                }
            }
            'e' | 'E' | 'f' | 'g' | 'G' => match count {
                None | Some(8) => (Kind::Float, letter, 8),
                Some(4) => (Kind::Float, letter, 4),
                _ => return Err(bad_count(&letter.to_string())),
            },
            's' => match (count, precision) {
                (Some(n), _) => (Kind::Str, 's', n),
                (None, Some(p)) => (Kind::Str, 's', p),
                (None, None) => return Err(fail("%s requires a precision or a byte count".into())),
            },
            '_' => {
                let second = rest.get(at + 1).copied().unwrap_or(0) as char;
                next += 1;
                match second {
                    'A' | 'a' => {
                        end |= second == 'A';
                        let base = rest.get(at + 2).copied().unwrap_or(0) as char;
                        next += 1;
                        if !matches!(base, 'd' | 'o' | 'x') {
                            return Err(bad_conversion(&format!("_{second}{base}")));
                        }
                        (Kind::Address, base, 0)
                    }
                    'c' | 'p' | 'u' => {
                        let kind = match second {
                            'c' => Kind::Escaped,
                            'p' => Kind::Printable,
                            _ => Kind::Named,
                        };
                        match count {
                            None | Some(1) => (kind, 'c', 1),
                            _ => return Err(bad_count(&format!("_{second}"))),
                        }
                    }
                    _ => return Err(bad_conversion(&format!("_{second}"))),
                }
            }
            other => return Err(bad_conversion(&other.to_string())),
        };
        if kind != Kind::Address && count.is_some() {
            conversions += 1;
            if conversions > 1 {
                return Err(fail("byte count with multiple conversion characters".into()));
            }
        }
        let width = spec
            .trim_start_matches([' ', '-', '0', '+', '#'])
            .split('.')
            .next()
            .unwrap_or("")
            .parse()
            .unwrap_or(0);
        parts.push(Part {
            text,
            conversion: Some(Conversion { kind, spec, letter, bytes, width }),
            no_space: false,
        });
        rest = &rest[next.min(rest.len())..];
    }
    Ok((parts, end))
}

/// The formats, ready for blocks of `block` bytes, as BSD hexdump makes
/// them ready: the last unit of a format shorter than the block, if it
/// reads data and has no count, is repeated to fill it, and a repeated
/// unit's trailing blank is left out the last time.
fn prepare(formats: &mut [Format], block: usize) {
    for format in formats.iter_mut() {
        let size: usize = format.iter().map(|u| u.size() * u.reps).sum();
        let units = format.len();
        for (n, unit) in format.iter_mut().enumerate() {
            let bytes = unit.size();
            if n + 1 == units && size < block && !unit.counted && bytes > 0 {
                unit.reps += (block - size) / bytes;
            }
            if unit.reps > 1
                && let Some(last) = unit.parts.last_mut()
            {
                let text_only = last.conversion.is_none();
                last.no_space = text_only && last.text.last().is_some_and(u8::is_ascii_whitespace);
            }
        }
    }
}

/// A value for printf(3).
enum Value<'a> {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Char(u8),
    Str(&'a [u8]),
}

/// printf(3)'s rendering of one value with `%{spec}{letter}`.
fn printf(spec: &str, letter: char, value: Value) -> Vec<u8> {
    let length = if matches!(value, Value::Signed(_) | Value::Unsigned(_)) { "ll" } else { "" };
    let Ok(format) = CString::new(format!("%{spec}{length}{letter}")) else { return Vec::new() };
    let string = match &value {
        Value::Str(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            CString::new(&bytes[..end]).unwrap_or_default()
        }
        _ => CString::default(),
    };
    let mut buffer = vec![0u8; 64];
    loop {
        let (out, size) = (buffer.as_mut_ptr().cast::<libc::c_char>(), buffer.len());
        // SAFETY: `format` is one conversion, of the type each value is
        // passed as, with flags, width and precision of SPEC's characters
        // only; snprintf writes at most `size` bytes to `buffer`.
        let n = unsafe {
            match value {
                Value::Signed(v) => {
                    libc::snprintf(out, size, format.as_ptr(), v as libc::c_longlong)
                }
                Value::Unsigned(v) => {
                    libc::snprintf(out, size, format.as_ptr(), v as libc::c_ulonglong)
                }
                Value::Float(v) => libc::snprintf(out, size, format.as_ptr(), v),
                Value::Char(c) => libc::snprintf(out, size, format.as_ptr(), libc::c_int::from(c)),
                Value::Str(_) => libc::snprintf(out, size, format.as_ptr(), string.as_ptr()),
            }
        };
        let Ok(n) = usize::try_from(n) else { return Vec::new() };
        if n < buffer.len() {
            buffer.truncate(n);
            return buffer;
        }
        buffer.resize(n + 1, 0);
    }
}

/// The control characters' names, for `%_u`.
const NAMES: [&str; 32] = [
    "nul", "soh", "stx", "etx", "eot", "enq", "ack", "bel", "bs", "ht", "lf", "vt", "ff", "cr",
    "so", "si", "dle", "dc1", "dc2", "dc3", "dc4", "nak", "syn", "etb", "can", "em", "sub", "esc",
    "fs", "gs", "rs", "us",
];

fn printable(b: u8) -> bool {
    (0x20..0x7f).contains(&b)
}

/// One conversion of `data`, which is the bytes it reads, at `address`.
fn convert(c: &Conversion, data: &[u8], address: u64) -> Vec<u8> {
    let mut word = [0u8; 8];
    word[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
    let unsigned = match c.bytes {
        1 => u64::from(word[0]),
        2 => u64::from(u16::from_ne_bytes([word[0], word[1]])),
        4 => u64::from(u32::from_ne_bytes([word[0], word[1], word[2], word[3]])),
        _ => u64::from_ne_bytes(word),
    };
    let signed = match c.bytes {
        1 => i64::from(word[0] as i8),
        2 => i64::from(unsigned as u16 as i16),
        4 => i64::from(unsigned as u32 as i32),
        _ => unsigned as i64,
    };
    let byte = word[0];
    let spec = c.spec.as_str();
    match c.kind {
        Kind::Address => printf(spec, c.letter, Value::Unsigned(address)),
        Kind::Signed => printf(spec, c.letter, Value::Signed(signed)),
        Kind::Unsigned => printf(spec, c.letter, Value::Unsigned(unsigned)),
        Kind::Float => {
            let value = if c.bytes == 4 {
                f64::from(f32::from_bits(unsigned as u32))
            } else {
                f64::from_bits(unsigned)
            };
            printf(spec, c.letter, Value::Float(value))
        }
        Kind::Char => printf(spec, 'c', Value::Char(byte)),
        Kind::Str => printf(spec, 's', Value::Str(data)),
        Kind::Escaped => {
            let escape = match byte {
                0 => Some("\\0".to_string()),
                7 => Some("\\a".into()),
                8 => Some("\\b".into()),
                12 => Some("\\f".into()),
                b'\n' => Some("\\n".into()),
                b'\r' => Some("\\r".into()),
                b'\t' => Some("\\t".into()),
                11 => Some("\\v".into()),
                b if printable(b) => None,
                b => Some(format!("{b:03o}")),
            };
            match escape {
                Some(text) => printf(spec, 's', Value::Str(text.as_bytes())),
                None => printf(spec, 'c', Value::Char(byte)),
            }
        }
        Kind::Printable => {
            printf(spec, 'c', Value::Char(if printable(byte) { byte } else { b'.' }))
        }
        Kind::Named => match byte {
            0..=0x1f => printf(spec, 's', Value::Str(NAMES[byte as usize].as_bytes())),
            0x7f => printf(spec, 's', Value::Str(b"del")),
            b if printable(b) => printf(spec, 'c', Value::Char(b)),
            b => printf(spec, 'x', Value::Unsigned(u64::from(b))),
        },
    }
}

/// Writes one block, at `address`, in every format; `end` is where the
/// data ended, in a last block it does not fill.
fn display(
    out: &mut dyn Write,
    formats: &[Format],
    block: &[u8],
    address: u64,
    end: Option<u64>,
) -> io::Result<()> {
    for format in formats {
        let mut at = address;
        let mut offset = 0;
        for unit in format {
            if unit.end {
                break;
            }
            for rep in (1..=unit.reps).rev() {
                for part in &unit.parts {
                    let mut text = part.text.as_slice();
                    if rep == 1 && part.no_space {
                        text = &text[..text.len() - 1];
                    }
                    out.write_all(text)?;
                    let Some(c) = &part.conversion else { continue };
                    if end.is_some_and(|end| at >= end) {
                        // Past the data: as many blanks as the field takes.
                        write!(out, "{:1$}", "", c.width)?;
                    } else {
                        let data = block
                            .get(offset..(offset + c.bytes).min(block.len()))
                            .unwrap_or_default();
                        out.write_all(&convert(c, data, at))?;
                    }
                    at += c.bytes as u64;
                    offset += c.bytes;
                }
            }
        }
    }
    Ok(())
}

/// The input: the files in turn (or standard input), after `skip` bytes,
/// for `length` bytes.
struct Input {
    files: Vec<OsString>,
    next: usize,
    current: Option<Box<dyn Read>>,
    skip: u64,
    length: Option<u64>,
    /// The bytes skipped, where the addresses start.
    skipped: u64,
    failed: bool,
}

impl Input {
    /// Opens the next input, skipping what is left to skip; false at the
    /// end of them.
    fn open_next(&mut self) -> bool {
        loop {
            if self.next >= self.files.len() {
                return false;
            }
            let name = self.files.get(self.next).cloned();
            self.next += 1;
            let mut reader: Box<dyn Read> = match name.as_ref().filter(|n| n.as_os_str() != "-") {
                Some(path) => match File::open(path) {
                    Ok(mut file) => {
                        if self.skip > 0
                            && let Some(meta) = file.metadata().ok().filter(|m| m.is_file())
                        {
                            let n = self.skip.min(meta.len());
                            if let Err(e) = file.seek(SeekFrom::Start(n)) {
                                self.report(path, e);
                                continue;
                            }
                            self.skip -= n;
                            self.skipped += n;
                        }
                        Box::new(file)
                    }
                    Err(e) => {
                        self.report(path, e);
                        continue;
                    }
                },
                None => Box::new(io::stdin()),
            };
            if self.skip > 0 {
                // Not a file to seek in: read past it.
                match io::copy(&mut (&mut reader).take(self.skip), &mut io::sink()) {
                    Ok(n) => {
                        self.skip -= n;
                        self.skipped += n;
                    }
                    Err(e) => {
                        self.report(name.as_ref().map_or("stdin".as_ref(), |n| n.as_os_str()), e);
                        continue;
                    }
                }
            }
            self.current = Some(reader);
            return true;
        }
    }

    fn report(&mut self, name: &std::ffi::OsStr, error: io::Error) {
        eprintln!("hexdump: {}: {}", name.to_string_lossy(), util::strip_os_error(&error));
        self.failed = true;
    }

    /// Fills as much of `buffer` as there is input for.
    fn fill(&mut self, buffer: &mut [u8]) -> usize {
        let want = match self.length {
            Some(left) => buffer.len().min(usize::try_from(left).unwrap_or(usize::MAX)),
            None => buffer.len(),
        };
        let mut got = 0;
        while got < want {
            if self.current.is_none() && !self.open_next() {
                break;
            }
            let Some(reader) = self.current.as_mut() else { break };
            match reader.read(&mut buffer[got..want]) {
                Ok(0) => self.current = None,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let name = self
                        .files
                        .get(self.next.wrapping_sub(1))
                        .cloned()
                        .unwrap_or_else(|| "stdin".into());
                    self.report(&name, e);
                    self.current = None;
                }
            }
        }
        if let Some(left) = self.length.as_mut() {
            *left -= got as u64;
        }
        got
    }
}

/// A size, with an optional unit: K, M, G... (KiB...) in powers of 1024,
/// KB, MB... in powers of 1000, and BSD's b for 512.
fn size(text: &str) -> Option<u64> {
    let split = if text.starts_with("0x") || text.starts_with("0X") {
        2 + text[2..].find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(text.len() - 2)
    } else {
        text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len())
    };
    let (number, unit) = text.split_at(split);
    let value = if let Some(hex) = number.strip_prefix("0x").or_else(|| number.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()?
    } else if number.len() > 1 && number.starts_with('0') {
        u64::from_str_radix(&number[1..], 8).ok()?
    } else {
        number.parse().ok()?
    };
    let multiplier: u64 = match unit {
        "" => 1,
        "b" => 512,
        _ => {
            let power = "KMGTPE".find(unit[..1].to_ascii_uppercase().as_str())? as u32 + 1;
            match &unit[1..] {
                "" | "iB" => 1024u64.pow(power),
                "B" => 1000u64.pow(power),
                _ => return None,
            }
        }
    };
    value.checked_mul(multiplier)
}

/// hd(1): hexdump(1), canonical unless a format is given.
pub fn hd(args: &mut Args) -> Result<i32, Fail> {
    run(args, true)
}

/// hexdump(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    run(args, false)
}

fn run(args: &mut Args, canonical: bool) -> Result<i32, Fail> {
    let mut texts: Vec<String> = Vec::new();
    let mut skip = 0;
    let mut length = None;
    let mut verbose = false;
    let mut files = Vec::new();
    let add = |texts: &mut Vec<String>, formats: &[&str]| {
        texts.extend(formats.iter().map(|f| f.to_string()))
    };
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('b') => add(&mut texts, &OCTAL_BYTES),
            Arg::Long(l) if l == "one-byte-octal" => add(&mut texts, &OCTAL_BYTES),
            Arg::Short('c') => add(&mut texts, &CHARACTERS),
            Arg::Long(l) if l == "one-byte-char" => add(&mut texts, &CHARACTERS),
            Arg::Short('C') => add(&mut texts, &CANONICAL),
            Arg::Long(l) if l == "canonical" => add(&mut texts, &CANONICAL),
            Arg::Short('d') => add(&mut texts, &DECIMAL),
            Arg::Long(l) if l == "two-bytes-decimal" => add(&mut texts, &DECIMAL),
            Arg::Short('o') => add(&mut texts, &OCTAL),
            Arg::Long(l) if l == "two-bytes-octal" => add(&mut texts, &OCTAL),
            Arg::Short('x') => add(&mut texts, &HEX),
            Arg::Long(l) if l == "two-bytes-hex" => add(&mut texts, &HEX),
            Arg::Short('e') => texts.push(args.text()?),
            Arg::Long(l) if l == "format" => texts.push(args.text()?),
            Arg::Short('f') => texts.extend(format_file(&args.value()?)?),
            Arg::Long(l) if l == "format-file" => texts.extend(format_file(&args.value()?)?),
            Arg::Short('n') => length = Some(size_value(args, "length")?),
            Arg::Long(l) if l == "length" => length = Some(size_value(args, "length")?),
            Arg::Short('s') => skip = size_value(args, "offset")?,
            Arg::Long(l) if l == "skip" => skip = size_value(args, "offset")?,
            Arg::Short('v') => verbose = true,
            Arg::Long(l) if l == "no-squeezing" => verbose = true,
            Arg::Short('L') => {}
            Arg::Long(l) if l == "color" => {
                let _ = args.optional_value();
            }
            Arg::Operand(file) => files.push(file),
            other => return Err(args.unknown(&other)),
        }
    }
    if files.is_empty() {
        files.push("-".into());
    }
    if texts.is_empty() {
        add(&mut texts, if canonical { &CANONICAL[..] } else { &DEFAULT[..] });
    }
    let mut formats = texts.iter().map(|t| parse(t)).collect::<Result<Vec<Format>, Fail>>()?;
    let block = formats
        .iter()
        .map(|f| f.iter().map(|u| u.size() * u.reps).sum::<usize>())
        .max()
        .unwrap_or(0);
    prepare(&mut formats, block);
    // The last unit that prints the end address is the one that does.
    let end_unit = formats.iter().flatten().rfind(|u| u.end).cloned();
    let mut input =
        Input { files, next: 0, current: None, skip, length, skipped: 0, failed: false };
    let mut out = BufWriter::new(io::stdout().lock());
    if block == 0 {
        return Ok(0);
    }
    let mut buffer = vec![0u8; block];
    let mut previous: Option<Vec<u8>> = None;
    let mut squeezed = false;
    let mut address = None;
    let mut end = None;
    loop {
        let n = input.fill(&mut buffer);
        let at = *address.get_or_insert(input.skipped);
        if n == 0 {
            break;
        }
        if n == block && !verbose && previous.as_deref() == Some(&buffer[..]) {
            if !squeezed {
                writeln!(out, "*")?;
                squeezed = true;
            }
            address = Some(at + block as u64);
            continue;
        }
        squeezed = false;
        if n < block {
            buffer[n..].fill(0);
            end = Some(at + n as u64);
        }
        display(&mut out, &formats, &buffer, at, end)?;
        if n < block {
            break;
        }
        previous = Some(buffer.clone());
        address = Some(at + block as u64);
    }
    if let Some(unit) = end_unit {
        let at = address.unwrap_or(input.skipped);
        let end = end.unwrap_or(at);
        if end > 0 {
            for part in &unit.parts {
                match &part.conversion {
                    None => out.write_all(&part.text)?,
                    Some(c) if c.kind == Kind::Address => {
                        out.write_all(&part.text)?;
                        out.write_all(&convert(c, &[], end))?;
                    }
                    Some(_) => {}
                }
            }
        }
    }
    out.flush()?;
    Ok(i32::from(input.failed))
}

fn size_value(args: &mut Args, what: &str) -> Result<u64, Fail> {
    let text = args.text()?;
    size(&text).ok_or_else(|| fail(format!("failed to parse {what}: '{text}'")))
}

/// The format strings in a file: one a line, but for blank lines and
/// lines that start with `#`.
fn format_file(path: &std::ffi::OsStr) -> Result<Vec<String>, Fail> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| util::failed(&std::path::Path::new(path).display(), e))?;
    Ok(text
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(texts: &[&str], data: &[u8], verbose: bool) -> String {
        let mut formats: Vec<Format> = texts.iter().map(|t| parse(t).unwrap()).collect();
        let block = formats
            .iter()
            .map(|f| f.iter().map(|u| u.size() * u.reps).sum::<usize>())
            .max()
            .unwrap();
        prepare(&mut formats, block);
        let mut out = Vec::new();
        let mut previous: Option<&[u8]> = None;
        for (n, chunk) in data.chunks(block).enumerate() {
            if !verbose && chunk.len() == block && previous == Some(chunk) {
                continue;
            }
            let mut buffer = chunk.to_vec();
            buffer.resize(block, 0);
            let end = (chunk.len() < block).then_some((n * block + chunk.len()) as u64);
            display(&mut out, &formats, &buffer, (n * block) as u64, end).unwrap();
            previous = Some(chunk);
        }
        String::from_utf8(out).unwrap()
    }

    const DATA: &[u8] = b"Hello world.hi there\0\0\x01\xff";

    #[test]
    fn named_formats() {
        assert_eq!(
            dump(&CANONICAL, DATA, false),
            "00000000  48 65 6c 6c 6f 20 77 6f  72 6c 64 2e 68 69 20 74  |Hello world.hi t|\n\
             00000010  68 65 72 65 00 00 01 ff                           |here....|\n"
        );
        assert_eq!(
            dump(&DEFAULT, DATA, false),
            "0000000 6548 6c6c 206f 6f77 6c72 2e64 6968 7420\n0000010 6568 6572 0000 ff01                    \n"
        );
        assert!(dump(&CHARACTERS, DATA, false).contains("  h   e   r   e  \\0  \\0 001 377"));
        assert!(dump(&DECIMAL, DATA, false).starts_with("0000000   25928   27756   08303"));
    }

    #[test]
    fn format_strings() {
        assert_eq!(
            dump(&["\"%07.7_ad: \" 4/1 \"%3u \" \"\\n\""], &DATA[..9], false).lines().last(),
            Some("0000008: 114            ")
        );
        assert_eq!(dump(&["2/1 \"%x\" \"%x\""], &DATA[..6], false), "4865206f6c6c");
        assert_eq!(dump(&["1/5 \"%s|\" \"\\n\""], &DATA[..10], false), "Hello|\n worl|\n");
        assert_eq!(
            dump(&["16/1 \"%_u \" \"\\n\""], &DATA[16..], false),
            "h e r e nul nul soh ff        \n"
        );
        assert_eq!(dump(&["\"[\\t]\" 1/1 \"%02X\""], b"\xab", false), "[\t]AB");
        assert!(parse("%x").is_err());
        assert!(parse("\"%q\"").is_err());
        assert!(parse("1/3 \"%x\"").is_err());
        assert!(parse("\"%5s\"").is_err());
        assert!(parse("1/1 \"%x %x\"").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(size("16"), Some(16));
        assert_eq!(size("0x10"), Some(16));
        assert_eq!(size("010"), Some(8));
        assert_eq!(size("1k"), Some(1024));
        assert_eq!(size("2KiB"), Some(2048));
        assert_eq!(size("1MB"), Some(1_000_000));
        assert_eq!(size("1b"), Some(512));
        assert_eq!(size("1q"), None);
    }
}
