//! The command language and line editor of `kdb`, the Radix kernel
//! debugger.
//!
//! `kdb` runs on the serial port. The kernel enters it at the end of boot,
//! at a breakpoint, or after a panic when booted with the `kdb` flag; see
//! radix-debug(7). This crate holds the parts that do not touch hardware:
//! [`LineEditor`] turns received bytes into lines, [`parse`] turns a line
//! into a [`Command`], and [`COMMANDS`] lists every command for `help` and
//! for the manual page check in `ob docs --check`. The kernel carries the
//! commands out.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// Description of one command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    /// The command's name.
    pub name: &'static str,
    /// Its arguments, as shown by `help`.
    pub args: &'static str,
    /// One-line description.
    pub summary: &'static str,
}

/// Every command, in the order `help` lists them.
pub const COMMANDS: &[CommandInfo] = &[
    CommandInfo { name: "help", args: "", summary: "list the commands" },
    CommandInfo {
        name: "log",
        args: "[count]",
        summary: "show the last count log messages, debug ones included (default 20)",
    },
    CommandInfo {
        name: "trace",
        args: "[on|off]",
        summary: "show the recorded trace points, or start or stop recording",
    },
    CommandInfo {
        name: "cpus",
        args: "",
        summary: "list the CPUs with their timer ticks and received IPIs",
    },
    CommandInfo {
        name: "mem",
        args: "",
        summary: "show the memory map, the kernel's reservations, the frame allocator's zones and \
                  the address-space layout",
    },
    CommandInfo {
        name: "heap",
        args: "",
        summary: "show the kernel heap's totals and its size classes",
    },
    CommandInfo {
        name: "caps",
        args: "",
        summary: "count the capability spaces, capabilities and kernel objects, by kind",
    },
    CommandInfo {
        name: "threads",
        args: "",
        summary: "list the threads with their state, policy, CPU time and resource domain",
    },
    CommandInfo {
        name: "sched",
        args: "",
        summary: "show every CPU's run queue, and the wait table's and RCU's counters",
    },
    CommandInfo {
        name: "domains",
        args: "",
        summary: "list the resource domains with their CPU weight, memory use and limit",
    },
    CommandInfo {
        name: "locks",
        args: "",
        summary: "show the lock checker's lock classes and the locks each CPU holds",
    },
    CommandInfo { name: "acpi", args: "", summary: "list the ACPI tables" },
    CommandInfo { name: "bt", args: "", summary: "show the backtrace where kdb was entered" },
    CommandInfo {
        name: "regs",
        args: "",
        summary: "show the registers saved when a breakpoint or fault entered kdb",
    },
    CommandInfo {
        name: "x",
        args: "address [length]",
        summary: "dump length bytes (default 64, at most 4096) of mapped kernel memory",
    },
    CommandInfo {
        name: "sym",
        args: "address|name",
        summary: "show the function containing an address, or a function's address",
    },
    CommandInfo { name: "panic", args: "", summary: "panic deliberately, to test the panic path" },
    CommandInfo { name: "reboot", args: "", summary: "restart the machine, keeping the crash log" },
    CommandInfo { name: "continue", args: "", summary: "leave kdb and carry on (also: c)" },
];

/// What `trace` should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceAction {
    /// Print the recorded events.
    Show,
    /// Start recording.
    On,
    /// Stop recording.
    Off,
}

/// The argument of `sym`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolQuery<'a> {
    /// Find the function containing this address.
    Address(u64),
    /// Find this function's address.
    Name(&'a str),
}

/// A parsed command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command<'a> {
    /// An empty line.
    Nothing,
    /// `help`.
    Help,
    /// `log [count]`.
    Log {
        /// Messages to show.
        count: usize,
    },
    /// `trace [on|off]`.
    Trace(TraceAction),
    /// `cpus`.
    Cpus,
    /// `mem`.
    Memory,
    /// `heap`.
    Heap,
    /// `caps`.
    Capabilities,
    /// `threads`.
    Threads,
    /// `sched`.
    Scheduler,
    /// `domains`.
    Domains,
    /// `locks`.
    Locks,
    /// `acpi`.
    Acpi,
    /// `bt`.
    Backtrace,
    /// `regs`.
    Registers,
    /// `x address [length]`.
    Examine {
        /// First byte.
        address: u64,
        /// Bytes to show.
        len: usize,
    },
    /// `sym address|name`.
    Symbol(SymbolQuery<'a>),
    /// `panic`.
    Panic,
    /// `reboot`.
    Reboot,
    /// `continue` or `c`.
    Continue,
}

/// Most bytes `x` shows at once.
pub const MAX_EXAMINE: usize = 4096;

/// Why a line is not a valid command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError<'a> {
    /// The first word is not a command.
    Unknown(&'a str),
    /// The arguments do not fit the command; holds its usage.
    Usage(&'static CommandInfo),
}

impl fmt::Display for ParseError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Unknown(word) => write!(f, "unknown command `{word}`; try `help`"),
            ParseError::Usage(info) if info.args.is_empty() => {
                write!(f, "usage: {} (no arguments)", info.name)
            }
            ParseError::Usage(info) => write!(f, "usage: {} {}", info.name, info.args),
        }
    }
}

/// Parses a number: hexadecimal with a `0x` prefix, decimal otherwise.
pub fn parse_number(text: &str) -> Option<u64> {
    let text = text.replace_underscores();
    match text.as_str().strip_prefix("0x").or_else(|| text.as_str().strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.as_str().parse().ok(),
    }
}

/// Parses an address: hexadecimal, with or without a `0x` prefix.
pub fn parse_address(text: &str) -> Option<u64> {
    let text = text.replace_underscores();
    let hex = text.as_str();
    let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(hex);
    u64::from_str_radix(hex, 16).ok()
}

/// Numbers may be written with `_` separators, such as `ffffffff_80100000`.
trait ReplaceUnderscores {
    fn replace_underscores(&self) -> Word;
}

impl ReplaceUnderscores for str {
    fn replace_underscores(&self) -> Word {
        let mut word = Word { bytes: [0; 32], len: 0 };
        for byte in self.bytes().filter(|&b| b != b'_') {
            if word.len == word.bytes.len() {
                // Too long to be a number; make it fail to parse.
                return Word { bytes: [b'!'; 32], len: 1 };
            }
            word.bytes[word.len] = byte;
            word.len += 1;
        }
        word
    }
}

struct Word {
    bytes: [u8; 32],
    len: usize,
}

impl Word {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("!")
    }
}

fn info(name: &str) -> &'static CommandInfo {
    COMMANDS.iter().find(|c| c.name == name).unwrap_or(&COMMANDS[0])
}

/// Parses one command line.
pub fn parse(line: &str) -> Result<Command<'_>, ParseError<'_>> {
    let mut words = line.split_ascii_whitespace();
    let Some(name) = words.next() else { return Ok(Command::Nothing) };
    let args: [Option<&str>; 3] = [words.next(), words.next(), words.next()];
    let usage = || ParseError::Usage(info(if name == "c" { "continue" } else { name }));
    let command = match (name, args) {
        ("help", [None, ..]) => Command::Help,
        ("log", [None, ..]) => Command::Log { count: 20 },
        ("log", [Some(count), None, _]) => Command::Log {
            count: parse_number(count).and_then(|n| usize::try_from(n).ok()).ok_or_else(usage)?,
        },
        ("trace", [None, ..]) => Command::Trace(TraceAction::Show),
        ("trace", [Some("on"), None, _]) => Command::Trace(TraceAction::On),
        ("trace", [Some("off"), None, _]) => Command::Trace(TraceAction::Off),
        ("cpus", [None, ..]) => Command::Cpus,
        ("mem", [None, ..]) => Command::Memory,
        ("heap", [None, ..]) => Command::Heap,
        ("caps", [None, ..]) => Command::Capabilities,
        ("threads", [None, ..]) => Command::Threads,
        ("sched", [None, ..]) => Command::Scheduler,
        ("domains", [None, ..]) => Command::Domains,
        ("locks", [None, ..]) => Command::Locks,
        ("acpi", [None, ..]) => Command::Acpi,
        ("bt", [None, ..]) => Command::Backtrace,
        ("regs", [None, ..]) => Command::Registers,
        ("x", [Some(address), len, None]) => {
            let address = parse_address(address).ok_or_else(usage)?;
            let len = match len {
                None => 64,
                Some(len) => parse_number(len).ok_or_else(usage)? as usize,
            };
            if len == 0 || len > MAX_EXAMINE {
                return Err(usage());
            }
            Command::Examine { address, len }
        }
        ("sym", [Some(arg), None, _]) => {
            if arg.contains("::") || arg.chars().any(|c| !c.is_ascii_hexdigit() && c != 'x') {
                Command::Symbol(SymbolQuery::Name(arg))
            } else {
                Command::Symbol(SymbolQuery::Address(parse_address(arg).ok_or_else(usage)?))
            }
        }
        ("panic", [None, ..]) => Command::Panic,
        ("reboot", [None, ..]) => Command::Reboot,
        ("continue" | "c", [None, ..]) => Command::Continue,
        _ if COMMANDS.iter().any(|c| c.name == name) || name == "c" => return Err(usage()),
        _ => return Err(ParseError::Unknown(name)),
    };
    Ok(command)
}

/// Writes `help` output.
pub fn write_help(out: &mut dyn fmt::Write) -> fmt::Result {
    for command in COMMANDS {
        let mut usage = Word { bytes: [b' '; 32], len: 0 };
        for byte in command.name.bytes().chain(" ".bytes()).chain(command.args.bytes()) {
            if usage.len < usage.bytes.len() {
                usage.bytes[usage.len] = byte;
                usage.len += 1;
            }
        }
        writeln!(out, "  {:<24} {}", usage.as_str().trim_end(), command.summary)?;
    }
    Ok(())
}

/// Turns bytes typed on a terminal into lines, echoing as it goes.
///
/// Handles backspace and delete, Ctrl-U (erase the line), Ctrl-C (abandon
/// the line) and the up arrow (recall the previous line). Other escape
/// sequences and control characters are ignored.
pub struct LineEditor<const N: usize> {
    line: [u8; N],
    len: usize,
    previous: [u8; N],
    previous_len: usize,
    /// The last call returned a line; start a new one.
    done: bool,
    /// The last byte was a carriage return; ignore a following line feed.
    after_cr: bool,
    escape: Escape,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// Saw ESC.
    Start,
    /// Saw ESC [ and maybe parameter bytes.
    Csi,
}

impl<const N: usize> Default for LineEditor<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LineEditor<N> {
    /// An empty editor.
    pub const fn new() -> Self {
        Self {
            line: [0; N],
            len: 0,
            previous: [0; N],
            previous_len: 0,
            done: false,
            after_cr: false,
            escape: Escape::None,
        }
    }

    /// The line typed so far.
    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.line[..self.len]).unwrap_or("")
    }

    /// Feeds one received byte. `echo` gets what to send back to the
    /// terminal. Returns the finished line when Enter is pressed (an empty
    /// line after Ctrl-C).
    pub fn feed(&mut self, byte: u8, echo: &mut dyn FnMut(&str)) -> Option<&str> {
        if self.done {
            self.done = false;
            self.len = 0;
        }
        let after_cr = core::mem::replace(&mut self.after_cr, false);
        match self.escape {
            Escape::Start => {
                self.escape = if byte == b'[' { Escape::Csi } else { Escape::None };
                return None;
            }
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = Escape::None;
                    if byte == b'A' {
                        self.recall(echo);
                    }
                }
                return None;
            }
            Escape::None => {}
        }
        match byte {
            b'\r' | b'\n' => {
                if byte == b'\n' && after_cr {
                    return None;
                }
                self.after_cr = byte == b'\r';
                echo("\r\n");
                if self.len > 0 {
                    self.previous[..self.len].copy_from_slice(&self.line[..self.len]);
                    self.previous_len = self.len;
                }
                self.done = true;
                return Some(self.line());
            }
            0x08 | 0x7f => {
                if self.len > 0 {
                    self.len -= 1;
                    echo("\x08 \x08");
                }
            }
            0x15 => self.erase(echo),
            0x03 => {
                echo("^C\r\n");
                self.len = 0;
                self.done = true;
                return Some("");
            }
            0x1b => self.escape = Escape::Start,
            0x20..=0x7e if self.len < N => {
                self.line[self.len] = byte;
                self.len += 1;
                let typed = [byte];
                echo(core::str::from_utf8(&typed).unwrap_or(""));
            }
            _ => {}
        }
        None
    }

    fn erase(&mut self, echo: &mut dyn FnMut(&str)) {
        for _ in 0..self.len {
            echo("\x08 \x08");
        }
        self.len = 0;
    }

    fn recall(&mut self, echo: &mut dyn FnMut(&str)) {
        self.erase(echo);
        self.line[..self.previous_len].copy_from_slice(&self.previous[..self.previous_len]);
        self.len = self.previous_len;
        echo(self.line());
    }
}

/// Writes a hex dump of `bytes`, which start at `address`: 16 bytes per
/// line with their printable ASCII.
pub fn write_hex_dump(out: &mut dyn fmt::Write, address: u64, bytes: &[u8]) -> fmt::Result {
    for (i, chunk) in bytes.chunks(16).enumerate() {
        write!(out, "{:016x} ", address + (i * 16) as u64)?;
        for column in 0..16 {
            match chunk.get(column) {
                Some(byte) => write!(out, " {byte:02x}")?,
                None => out.write_str("   ")?,
            }
            if column == 7 {
                out.write_char(' ')?;
            }
        }
        out.write_str("  |")?;
        for &byte in chunk {
            out.write_char(if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            })?;
        }
        out.write_str("|\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse("  "), Ok(Command::Nothing));
        assert_eq!(parse("help"), Ok(Command::Help));
        assert_eq!(parse("log"), Ok(Command::Log { count: 20 }));
        assert_eq!(parse("log 0x10"), Ok(Command::Log { count: 16 }));
        assert_eq!(parse("trace on"), Ok(Command::Trace(TraceAction::On)));
        assert_eq!(parse("trace"), Ok(Command::Trace(TraceAction::Show)));
        assert_eq!(
            parse("x ffffffff_80100000 32"),
            Ok(Command::Examine { address: 0xffff_ffff_8010_0000, len: 32 })
        );
        assert_eq!(parse("x 0x1000"), Ok(Command::Examine { address: 0x1000, len: 64 }));
        assert_eq!(
            parse("sym 0xffffffff80100000"),
            Ok(Command::Symbol(SymbolQuery::Address(0xffff_ffff_8010_0000)))
        );
        assert_eq!(
            parse("sym radix::kernel_main"),
            Ok(Command::Symbol(SymbolQuery::Name("radix::kernel_main")))
        );
        assert_eq!(parse("c"), Ok(Command::Continue));
    }

    #[test]
    fn parses_scheduler_commands() {
        assert_eq!(parse("threads"), Ok(Command::Threads));
        assert_eq!(parse(" sched "), Ok(Command::Scheduler));
        assert_eq!(parse("domains"), Ok(Command::Domains));
        assert_eq!(parse("locks"), Ok(Command::Locks));
        assert_eq!(parse("threads all").unwrap_err().to_string(), "usage: threads (no arguments)");
    }

    #[test]
    fn reports_errors() {
        assert_eq!(
            parse("frobnicate").unwrap_err().to_string(),
            "unknown command `frobnicate`; try `help`"
        );
        assert_eq!(parse("log many").unwrap_err().to_string(), "usage: log [count]");
        assert_eq!(parse("x").unwrap_err().to_string(), "usage: x address [length]");
        assert!(parse("x 1000 99999").is_err());
        assert!(parse("trace sideways").is_err());
        assert_eq!(parse("c now").unwrap_err().to_string(), "usage: continue (no arguments)");
    }

    #[test]
    fn help_lists_every_command() {
        let mut out = String::new();
        write_help(&mut out).unwrap();
        for command in COMMANDS {
            assert!(out.contains(command.name));
            assert!(out.contains(command.summary));
        }
        for (i, command) in COMMANDS.iter().enumerate() {
            assert!(COMMANDS[i + 1..].iter().all(|other| other.name != command.name));
        }
    }

    fn feed_all<const N: usize>(editor: &mut LineEditor<N>, input: &[u8]) -> (Vec<String>, String) {
        let mut lines = Vec::new();
        let mut echoed = String::new();
        for &byte in input {
            if let Some(line) = editor.feed(byte, &mut |s| echoed.push_str(s)) {
                lines.push(line.to_string());
            }
        }
        (lines, echoed)
    }

    #[test]
    fn edits_lines() {
        let mut editor = LineEditor::<16>::new();
        let (lines, echoed) = feed_all(&mut editor, b"hepl\x7f\x7flp\r\nlog\x15cpus\n");
        assert_eq!(lines, ["help", "cpus"]);
        assert!(echoed.starts_with("hepl\x08 \x08\x08 \x08lp\r\n"));

        let (lines, _) = feed_all(&mut editor, b"\x1b[Ax\x1b[D\r");
        assert_eq!(lines, ["cpusx"]);
        let (lines, echoed) = feed_all(&mut editor, b"abc\x03");
        assert_eq!(lines, [""]);
        assert!(echoed.ends_with("^C\r\n"));
    }

    #[test]
    fn long_lines_are_cut() {
        let mut editor = LineEditor::<4>::new();
        let (lines, _) = feed_all(&mut editor, b"abcdefg\r");
        assert_eq!(lines, ["abcd"]);
    }

    #[test]
    fn hex_dump_layout() {
        let mut out = String::new();
        write_hex_dump(&mut out, 0x1000, b"Radix kernel\x00\x01\x02\x03AB").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "0000000000001000  52 61 64 69 78 20 6b 65  72 6e 65 6c 00 01 02 03  |Radix kernel....|"
        );
        assert_eq!(
            lines[1],
            "0000000000001010  41 42                                             |AB|"
        );
        assert_eq!(parse_number("12"), Some(12));
        assert_eq!(parse_number("0x1_0"), Some(16));
        assert_eq!(parse_address("dead_beef"), Some(0xdead_beef));
        assert_eq!(parse_address("ffffffffffffffffff"), None);
    }
}
