// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! xxd(1): a hex dump of a file, or its bytes back from one (`-r`), with
//! the options and layouts of Vim's xxd: grouped hex and text, plain hex
//! (`-p`), bits (`-b`), little-endian groups (`-e`) and a C array (`-i`).
//! Written for Dukovo; no code of Vim's xxd is in it.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::args::{Arg, Args};
use crate::util::{self, Fail, usage};

/// The layouts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Normal,
    Plain,
    Bits,
    Include,
    Little,
}

struct Options {
    layout: Layout,
    columns: Option<usize>,
    group: Option<usize>,
    upper: bool,
    length: Option<u64>,
    seek: Option<i64>,
    offset: u64,
    autoskip: bool,
    decimal: bool,
    capitalize: bool,
    name: Option<String>,
    reverse: bool,
}

/// xxd(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut o = Options {
        layout: Layout::Normal,
        columns: None,
        group: None,
        upper: false,
        length: None,
        seek: None,
        offset: 0,
        autoskip: false,
        decimal: false,
        capitalize: false,
        name: None,
        reverse: false,
    };
    let mut files: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('p') | Arg::Long(_) if is(&arg, 'p', &["ps", "postscript", "plain"]) => {
                o.layout = Layout::Plain;
            }
            Arg::Short('b') | Arg::Long(_) if is(&arg, 'b', &["bits"]) => o.layout = Layout::Bits,
            Arg::Short('i') | Arg::Long(_) if is(&arg, 'i', &["include"]) => {
                o.layout = Layout::Include;
            }
            Arg::Short('e') => o.layout = Layout::Little,
            Arg::Short('c') | Arg::Long(_) if is(&arg, 'c', &["cols"]) => {
                let text = args.text()?;
                if text.starts_with('-') {
                    return Err(too_wide());
                }
                let n =
                    parse_number(&text).ok_or_else(|| usage(format!("invalid number '{text}'")))?;
                o.columns = Some(usize::try_from(n).unwrap_or(usize::MAX));
            }
            Arg::Short('g') | Arg::Long(_) if is(&arg, 'g', &["groupsize"]) => {
                o.group = Some(number(args)? as usize);
            }
            Arg::Short('l') | Arg::Long(_) if is(&arg, 'l', &["len"]) => {
                o.length = Some(number(args)?);
            }
            Arg::Short('o') => o.offset = number(args)?,
            Arg::Short('s') | Arg::Long(_) if is(&arg, 's', &["seek"]) => {
                let text = args.text()?;
                let body = text.trim_start_matches('+');
                let (negative, body) = match body.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, body),
                };
                let n = parse_number(body)
                    .ok_or_else(|| usage(format!("invalid seek offset '{text}'")))?;
                // Too big an offset is the biggest one, as strtol gives.
                let n = i64::try_from(n).unwrap_or(i64::MAX);
                o.seek = Some(if negative { -n } else { n });
            }
            Arg::Short('u') => o.upper = true,
            Arg::Short('a') | Arg::Long(_) if is(&arg, 'a', &["autoskip"]) => o.autoskip = true,
            Arg::Short('d') => o.decimal = true,
            Arg::Short('C') | Arg::Long(_) if is(&arg, 'C', &["capitalize"]) => o.capitalize = true,
            Arg::Short('n') | Arg::Long(_) if is(&arg, 'n', &["name"]) => {
                o.name = Some(args.text()?);
            }
            Arg::Short('r') | Arg::Long(_) if is(&arg, 'r', &["revert"]) => o.reverse = true,
            Arg::Short('R') => {
                let _ = args.text()?;
            }
            Arg::Short('v') | Arg::Long(_) if is(&arg, 'v', &["version"]) => {
                println!("xxd from Dukovo's systools {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            Arg::Short('h') | Arg::Long(_) if is(&arg, 'h', &["help"]) => {
                println!(
                    "usage: xxd [-abCdeiprsu] [-c COLS] [-g BYTES] [-l LEN] [-n NAME] [-o OFF] [-s [+][-]SEEK] [INFILE [OUTFILE]]"
                );
                return Ok(0);
            }
            Arg::Operand(file) if files.len() < 2 => files.push(file),
            other => return Err(args.unknown(&other)),
        }
    }
    // Plain hex and C take lines of any length; the others up to 256
    // bytes.
    if o.columns.is_some_and(|c| c > 256) && !matches!(o.layout, Layout::Plain | Layout::Include) {
        return Err(too_wide());
    }
    let input_name = files.first().filter(|f| f.as_os_str() != "-").cloned();
    // Where in the input the dump starts, which its offsets count from.
    let mut start = 0u64;
    let mut input: Box<dyn Read> = match &input_name {
        Some(path) => {
            let mut file = File::open(path).map_err(|e| failed(2, path, e))?;
            if let Some(seek) = o.seek.filter(|_| !o.reverse) {
                let to = if seek < 0 { SeekFrom::End(seek) } else { SeekFrom::Start(seek as u64) };
                start = file.seek(to).map_err(|e| failed(4, path, e))?;
            }
            Box::new(file)
        }
        None => {
            let mut stdin = io::stdin().lock();
            if let Some(seek) = o.seek.filter(|_| !o.reverse) {
                if seek < 0 {
                    return Err(usage("a seek from the end needs a file".into()));
                }
                io::copy(&mut (&mut stdin).take(seek as u64), &mut io::sink())?;
                start = seek as u64;
            }
            Box::new(stdin)
        }
    };
    let output_name = files.get(1).filter(|f| f.as_os_str() != "-");
    if o.reverse {
        let mut input = BufReader::new(input);
        // Vim's xxd exits 5 when it cannot go back.
        let backwards = |e: io::Error| match e.kind() {
            io::ErrorKind::NotSeekable => util::fail_with(5, e.to_string()),
            _ => e.into(),
        };
        match output_name {
            // A file is patched, as Vim's xxd patches it: not cut short,
            // and written where each line's offset says.
            Some(path) => {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .map_err(|e| failed(3, path, e))?;
                let mut output = io::BufWriter::new(file);
                reverse(&mut input, &mut output, &o).map_err(backwards)?;
                output.flush()?;
            }
            None => {
                let mut output = io::BufWriter::new(io::stdout().lock());
                reverse(&mut input, &mut output, &o).map_err(backwards)?;
                output.flush()?;
            }
        }
        return Ok(0);
    }
    let output: Box<dyn Write> = match output_name {
        Some(path) => Box::new(File::create(path).map_err(|e| failed(3, path, e))?),
        None => Box::new(io::stdout().lock()),
    };
    let mut output = io::BufWriter::new(output);
    if let Some(length) = o.length {
        input = Box::new(input.take(length));
    }
    dump(&mut input, &mut output, &o, start, input_name.as_ref())?;
    output.flush()?;
    Ok(0)
}

/// A file that could not be opened or sought in, with Vim's xxd's exit
/// status for it: 2 for the input, 3 for the output, 4 for a seek.
fn failed(code: i32, path: &OsString, e: io::Error) -> Fail {
    let what = util::failed(&Path::new(path).display(), e);
    util::fail_with(code, what.message)
}

/// A `-c` that is negative, or over 256 where it cannot be.
fn too_wide() -> Fail {
    util::fail("invalid number of columns (max. 256).".into())
}

/// Whether `arg` is short option `c` or one of the long `names`.
fn is(arg: &Arg, c: char, names: &[&str]) -> bool {
    match arg {
        Arg::Short(s) => *s == c,
        Arg::Long(l) => names.contains(&l.as_str()),
        Arg::Operand(_) => false,
    }
}

/// A number in decimal, or in hex with `0x`, or in octal with `0`.
fn parse_number(text: &str) -> Option<u64> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else if text.len() > 1 && text.starts_with('0') {
        u64::from_str_radix(&text[1..], 8).ok()
    } else {
        text.parse().ok()
    }
}

fn number(args: &mut Args) -> Result<u64, Fail> {
    let text = args.text()?;
    parse_number(&text).ok_or_else(|| usage(format!("invalid number '{text}'")))
}

/// Reads up to `buffer.len()` bytes, less only at the end.
fn fill(input: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buffer.len() {
        match input.read(&mut buffer[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Writes the dump.
fn dump(
    input: &mut dyn Read,
    out: &mut dyn Write,
    o: &Options,
    start: u64,
    name: Option<&OsString>,
) -> io::Result<()> {
    // `-c 0` is the layout's default, but for plain hex one long line.
    let columns = match o.columns {
        Some(n) if n > 0 => n,
        _ => match o.layout {
            Layout::Plain => 30,
            Layout::Bits => 6,
            Layout::Include => 12,
            _ => 16,
        },
    };
    let group = o.group.unwrap_or(match o.layout {
        Layout::Bits => 1,
        Layout::Little => 4,
        _ => 2,
    });
    let hex = |b: u8| if o.upper { format!("{b:02X}") } else { format!("{b:02x}") };
    if o.layout == Layout::Include {
        let var = name.map(|n| {
            let text: String = n
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let text = if text.starts_with(|c: char| c.is_ascii_digit()) {
                format!("__{text}")
            } else {
                text
            };
            if o.capitalize { text.to_ascii_uppercase() } else { text }
        });
        let var = o.name.clone().or(var);
        if let Some(var) = &var {
            writeln!(out, "unsigned char {var}[] = {{")?;
        }
        let mut buffer = vec![0u8; columns];
        let mut total = 0u64;
        let mut first = true;
        loop {
            let n = fill(input, &mut buffer)?;
            if n == 0 {
                break;
            }
            if !first {
                writeln!(out, ",")?;
            }
            first = false;
            let items: Vec<String> = buffer[..n].iter().map(|&b| format!("0x{}", hex(b))).collect();
            write!(out, "  {}", items.join(", "))?;
            total += n as u64;
        }
        if !first {
            writeln!(out)?;
        }
        if let Some(var) = &var {
            let len = if o.capitalize { "LEN" } else { "len" };
            writeln!(out, "}};\nunsigned int {var}_{len} = {total};")?;
        }
        return Ok(());
    }
    if o.layout == Layout::Plain {
        let one_line = o.columns == Some(0);
        let mut buffer = vec![0u8; if one_line { 4096 } else { columns }];
        loop {
            let n = fill(input, &mut buffer)?;
            if n == 0 {
                break;
            }
            let text: String = buffer[..n].iter().map(|&b| hex(b)).collect();
            if one_line {
                write!(out, "{text}")?;
            } else {
                writeln!(out, "{text}")?;
            }
        }
        // Vim's xxd ends the one line even when there was nothing.
        if one_line {
            writeln!(out)?;
        }
        return Ok(());
    }
    let line_text = |at: u64, line: &[u8]| -> String {
        let mut text = if o.decimal { format!("{at:08}: ") } else { format!("{at:08x}: ") };
        let width_of = |bytes: usize| -> usize {
            let digits = if o.layout == Layout::Bits { 8 } else { 2 };
            let groups = if group == 0 { 1 } else { bytes.div_ceil(group) };
            // Little-endian groups are always whole, padded on the left.
            let bytes =
                if o.layout == Layout::Little && group > 0 { groups * group } else { bytes };
            bytes * digits + groups.saturating_sub(1)
        };
        let mut hex_part = String::new();
        let groups: Vec<&[u8]> = if group == 0 { vec![line] } else { line.chunks(group).collect() };
        for (i, chunk) in groups.iter().enumerate() {
            if i > 0 {
                hex_part.push(' ');
            }
            match o.layout {
                Layout::Bits => {
                    let bits: Vec<String> = chunk.iter().map(|b| format!("{b:08b}")).collect();
                    hex_part.push_str(&bits.join(""));
                }
                Layout::Little => {
                    // A short last group lines up on the right.
                    let pad = (group - chunk.len()) * 2;
                    hex_part.extend(std::iter::repeat_n(' ', pad));
                    for &b in chunk.iter().rev() {
                        hex_part.push_str(&hex(b));
                    }
                }
                _ => {
                    for &b in *chunk {
                        hex_part.push_str(&hex(b));
                    }
                }
            }
        }
        let full = width_of(columns);
        text.push_str(&hex_part);
        text.extend(std::iter::repeat_n(' ', full.saturating_sub(hex_part.len())));
        // xxd leaves a space more after little-endian groups.
        text.push_str(if o.layout == Layout::Little { "   " } else { "  " });
        text.extend(line.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }));
        text
    };
    // With -a a run of whole lines of zeros shows its first line, then a
    // `*` for the rest, or the one line itself when only one is left;
    // and the last line always shows, so the length does, as with Vim's
    // xxd.
    let zero_line = vec![0u8; columns];
    let mut buffer = vec![0u8; columns];
    let mut next = vec![0u8; columns];
    let mut n = fill(input, &mut buffer)?;
    let mut at = start + o.offset;
    let mut zeros_before = false;
    let mut skipped = 0u64;
    let mut skipped_at = 0;
    while n > 0 {
        let following = fill(input, &mut next)?;
        let line = &buffer[..n];
        let zeros = n == columns && line.iter().all(|&b| b == 0);
        if o.autoskip && zeros && zeros_before && following > 0 {
            if skipped == 0 {
                skipped_at = at;
            }
            skipped += 1;
        } else {
            match skipped {
                0 => {}
                1 => writeln!(out, "{}", line_text(skipped_at, &zero_line))?,
                _ => writeln!(out, "*")?,
            }
            skipped = 0;
            writeln!(out, "{}", line_text(at, line))?;
        }
        zeros_before = zeros;
        at += n as u64;
        std::mem::swap(&mut buffer, &mut next);
        n = following;
    }
    Ok(())
}

/// The value of a hex digit.
fn digit(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Where `-r` writes: it moves on to each line's offset.
trait Patch: Write {
    /// Goes from offset `at` to `to`. Output that cannot seek, such as a
    /// pipe, goes forwards only, with zeros, as Vim's xxd does.
    fn move_to(&mut self, at: u64, to: u64) -> io::Result<()> {
        if to < at {
            return Err(io::Error::new(
                io::ErrorKind::NotSeekable,
                "Sorry, cannot seek backwards.",
            ));
        }
        io::copy(&mut io::repeat(0).take(to - at), self)?;
        Ok(())
    }
}

impl Patch for Vec<u8> {}

impl Patch for io::BufWriter<io::StdoutLock<'_>> {}

impl Patch for io::BufWriter<File> {
    fn move_to(&mut self, _: u64, to: u64) -> io::Result<()> {
        self.seek(SeekFrom::Start(to)).map(drop)
    }
}

/// Turns a dump back into bytes: a normal one by the offsets it gives,
/// or with `-p` plain hex.
fn reverse(input: &mut dyn BufRead, out: &mut dyn Patch, o: &Options) -> io::Result<()> {
    let mut written = 0u64;
    let base = o.seek.unwrap_or(0).max(0) as u64;
    if o.layout == Layout::Plain && base > 0 {
        out.move_to(0, base)?;
        written = base;
    }
    let mut pending: Option<u8> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let mut bytes = Vec::new();
        let body: &[u8] = if o.layout == Layout::Plain {
            &line
        } else {
            let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
            let text = String::from_utf8_lossy(&line[..colon]);
            let at = u64::from_str_radix(text.trim(), 16).unwrap_or(written);
            let at = at.saturating_sub(o.offset) + base;
            if at != written {
                out.move_to(written, at)?;
                written = at;
            }
            pending = None;
            // The hex ends where the text column begins: two spaces.
            let rest = &line[colon + 1..];
            let end = rest.windows(2).position(|w| w == b"  ").unwrap_or(rest.len());
            &line[colon + 1..colon + 1 + end]
        };
        for &b in body {
            let Some(d) = digit(b) else {
                if o.layout != Layout::Plain && !b.is_ascii_whitespace() {
                    break;
                }
                continue;
            };
            match pending.take() {
                Some(high) => bytes.push(high << 4 | d),
                None => pending = Some(d),
            }
        }
        out.write_all(&bytes)?;
        written += bytes.len() as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(layout: Layout) -> Options {
        Options {
            layout,
            columns: None,
            group: None,
            upper: false,
            length: None,
            seek: None,
            offset: 0,
            autoskip: false,
            decimal: false,
            capitalize: false,
            name: None,
            reverse: false,
        }
    }

    fn dumped(data: &[u8], o: &Options, name: Option<&str>) -> String {
        let mut out = Vec::new();
        let name = name.map(OsString::from);
        dump(&mut &data[..], &mut out, o, 0, name.as_ref()).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn dumps_as_xxd_does() {
        assert_eq!(
            dumped(b"Hello\n", &options(Layout::Normal), None),
            "00000000: 4865 6c6c 6f0a                           Hello.\n"
        );
        assert_eq!(
            dumped(b"Hello world.hi there", &options(Layout::Normal), None),
            "00000000: 4865 6c6c 6f20 776f 726c 642e 6869 2074  Hello world.hi t\n\
             00000010: 6865 7265                                here\n"
        );
        assert_eq!(dumped(b"\x01\xff", &options(Layout::Plain), None), "01ff\n");
        assert_eq!(
            dumped(b"A", &options(Layout::Bits), None),
            "00000000: 01000001                                               A\n"
        );
        assert_eq!(
            dumped(b"ab", &options(Layout::Include), Some("my-file.bin")),
            "unsigned char my_file_bin[] = {\n  0x61, 0x62\n};\nunsigned int my_file_bin_len = 2;\n"
        );
        assert_eq!(
            dumped(b"Hello", &options(Layout::Little), None),
            "00000000: 6c6c6548       6f                     Hello\n"
        );
    }

    /// What Vim's xxd 2023-10-25 prints for these, with -a and -c.
    #[test]
    fn skips_zeros_and_takes_columns_as_xxd_does() {
        let mut o = options(Layout::Normal);
        o.autoskip = true;
        o.columns = Some(8);
        let zeros = |n| vec![0u8; n];
        let line = |at: u32| format!("{at:08x}: 0000 0000 0000 0000  ........\n");
        // A lone line of zeros after the first shows in full.
        assert_eq!(dumped(&zeros(16), &o, None), line(0) + &line(8));
        let mut data = zeros(16);
        data.push(b'a');
        assert_eq!(
            dumped(&data, &o, None),
            line(0) + &line(8) + "00000010: 61                   a\n"
        );
        data.insert(0, 0);
        data.splice(0..0, zeros(7));
        assert_eq!(dumped(&data, &o, None), line(0) + "*\n" + "00000018: 61                   a\n");
        // More than one is a star, and the last line always shows.
        assert_eq!(dumped(&zeros(32), &o, None), line(0) + "*\n" + &line(24));
        assert_eq!(
            dumped(&zeros(36), &o, None),
            line(0) + "*\n" + "00000020: 0000 0000            ....\n"
        );
        let mut data = b"x".repeat(8);
        data.extend(zeros(32));
        data.push(b'z');
        assert_eq!(
            dumped(&data, &o, None),
            "00000000: 7878 7878 7878 7878  xxxxxxxx\n".to_string()
                + &line(8)
                + "*\n"
                + "00000028: 7a                   z\n"
        );
        // -c 0 is the default width, but one line of plain hex.
        o.autoskip = false;
        o.columns = Some(0);
        assert_eq!(
            dumped(b"ab", &o, None),
            "00000000: 6162                                     ab\n"
        );
        let mut plain = options(Layout::Plain);
        plain.columns = Some(0);
        assert_eq!(dumped(&[0xab; 40], &plain, None), "ab".repeat(40) + "\n");
        assert_eq!(dumped(b"", &plain, None), "\n");
        plain.columns = Some(300);
        assert_eq!(dumped(&[1; 300], &plain, None), "01".repeat(300) + "\n");
    }

    #[test]
    fn reverses_its_dumps() {
        let data: Vec<u8> = (0..=255u8).collect();
        for layout in [Layout::Normal, Layout::Plain] {
            let o = options(layout);
            let text = dumped(&data, &o, None);
            let mut back = Vec::new();
            reverse(&mut text.as_bytes(), &mut back, &o).unwrap();
            assert_eq!(back, data);
        }
    }
}
