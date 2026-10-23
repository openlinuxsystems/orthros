// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The line discipline: POSIX's general terminal interface, as Linux's
//! N_TTY behaves for the flags it supports.

use radix_abi::signal::{SIGINT, SIGQUIT, SIGTSTP};
use radix_abi::terminal::{
    ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, ECHONL, ICANON, ICRNL, IEXTEN, IGNCR, INLCR, ISIG, ISTRIP,
    IUTF8, IXANY, IXON, NOFLSH, OCRNL, ONLCR, OPOST, Termios, VEOF, VEOL, VEOL2, VERASE, VINTR,
    VKILL, VLNEXT, VMIN, VQUIT, VREPRINT, VSTART, VSTOP, VSUSP, VTIME, VWERASE,
};

use crate::Ring;

/// The size of the C library's `struct termios`.
pub const TERMIOS_SIZE: usize = Termios::SIZE;

/// The value of a `c_cc` entry that is turned off (POSIX's
/// `_POSIX_VDISABLE`).
const DISABLED: u8 = 0;

/// The most complete lines canonical input holds before more are read.
const LINES: usize = 64;

/// A terminal's settings: the fields of `struct termios`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Input flags.
    pub iflag: u32,
    /// Output flags.
    pub oflag: u32,
    /// Control flags.
    pub cflag: u32,
    /// Local flags.
    pub lflag: u32,
    /// The special characters.
    pub cc: [u8; 32],
    /// The input speed.
    pub ispeed: u32,
    /// The output speed.
    pub ospeed: u32,
}

/// Linux's B38400, the speed a terminal reports when it has none.
const B38400: u32 = 0o17;

impl Default for Settings {
    fn default() -> Settings {
        Settings::new()
    }
}

impl Settings {
    /// What a new terminal starts with: Linux's defaults, with UTF-8 input.
    pub const fn new() -> Settings {
        let mut cc = [DISABLED; 32];
        cc[VINTR as usize] = 0x03;
        cc[VQUIT as usize] = 0x1c;
        cc[VERASE as usize] = 0x7f;
        cc[VKILL as usize] = 0x15;
        cc[VEOF as usize] = 0x04;
        cc[VTIME as usize] = 0;
        cc[VMIN as usize] = 1;
        cc[VSTART as usize] = 0x11;
        cc[VSTOP as usize] = 0x13;
        cc[VSUSP as usize] = 0x1a;
        cc[VREPRINT as usize] = 0x12;
        cc[VWERASE as usize] = 0x17;
        cc[VLNEXT as usize] = 0x16;
        Settings {
            iflag: ICRNL | IXON | IUTF8,
            oflag: OPOST | ONLCR,
            // CS8 | CREAD | HUPCL, at 38400 baud.
            cflag: 0o60 | 0o200 | 0o2000 | B38400,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            cc,
            ispeed: B38400,
            ospeed: B38400,
        }
    }

    /// The settings as the C library's `struct termios`.
    pub fn to_bytes(&self) -> [u8; TERMIOS_SIZE] {
        let mut out = [0u8; TERMIOS_SIZE];
        let mut put = |at: usize, value: u32| out[at..at + 4].copy_from_slice(&value.to_le_bytes());
        put(Termios::OFFSET_IFLAG, self.iflag);
        put(Termios::OFFSET_OFLAG, self.oflag);
        put(Termios::OFFSET_CFLAG, self.cflag);
        put(Termios::OFFSET_LFLAG, self.lflag);
        put(Termios::OFFSET_ISPEED, self.ispeed);
        put(Termios::OFFSET_OSPEED, self.ospeed);
        out[Termios::OFFSET_CC..Termios::OFFSET_CC + 32].copy_from_slice(&self.cc);
        out
    }

    /// The settings a `struct termios` holds.
    pub fn from_bytes(bytes: &[u8; TERMIOS_SIZE]) -> Settings {
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let mut cc = [0u8; 32];
        cc.copy_from_slice(&bytes[Termios::OFFSET_CC..Termios::OFFSET_CC + 32]);
        Settings {
            iflag: word(Termios::OFFSET_IFLAG),
            oflag: word(Termios::OFFSET_OFLAG),
            cflag: word(Termios::OFFSET_CFLAG),
            lflag: word(Termios::OFFSET_LFLAG),
            cc,
            ispeed: word(Termios::OFFSET_ISPEED),
            ospeed: word(Termios::OFFSET_OSPEED),
        }
    }

    fn local(&self, flag: u32) -> bool {
        self.lflag & flag != 0
    }

    fn input(&self, flag: u32) -> bool {
        self.iflag & flag != 0
    }

    /// Whether `byte` is the special character at index `index`, and that
    /// one is on.
    fn is(&self, byte: u8, index: u32) -> bool {
        let special = self.cc[index as usize];
        special != DISABLED && byte == special
    }

    /// Whether input is edited a line at a time.
    pub fn canonical(&self) -> bool {
        self.local(ICANON)
    }

    /// The non-canonical read minimum and timeout (tenths of a second).
    pub fn min_time(&self) -> (u8, u8) {
        (self.cc[VMIN as usize], self.cc[VTIME as usize])
    }
}

/// One terminal's line discipline.
pub struct Ldisc<B> {
    /// The settings.
    pub settings: Settings,
    /// Input: complete lines, then (canonical) the line being edited.
    input: Ring<B>,
    /// The lengths of the complete lines, oldest first, from `lines_head`;
    /// in canonical mode only.
    lines: [u16; LINES],
    lines_head: usize,
    lines_count: usize,
    /// The bytes of the complete lines.
    complete: usize,
    /// The next byte is taken as it is (after LNEXT).
    literal: bool,
    /// The cursor's column on the screen, as far as the output shows it.
    column: usize,
    /// The column the line being edited started at.
    edit_column: usize,
    /// Output is stopped (IXON's STOP character).
    pub output_stopped: bool,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> Ldisc<B> {
    /// A line discipline with the default settings, whose input goes in
    /// `buf` (4 KiB is Linux's).
    pub fn new(buf: B) -> Ldisc<B> {
        Ldisc {
            settings: Settings::new(),
            input: Ring::new(buf),
            lines: [0; LINES],
            lines_head: 0,
            lines_count: 0,
            complete: 0,
            literal: false,
            column: 0,
            edit_column: 0,
            output_stopped: false,
        }
    }

    /// Changes the settings. Leaving canonical mode makes what was typed
    /// readable as it is; entering it makes it the line being edited.
    pub fn set(&mut self, settings: Settings) {
        let was = self.settings.canonical();
        self.settings = settings;
        if was != settings.canonical() {
            self.lines_count = 0;
            self.lines_head = 0;
            self.complete = if settings.canonical() { 0 } else { self.input.len() };
            self.edit_column = self.column;
        }
        if settings.iflag & IXON == 0 {
            self.output_stopped = false;
        }
    }

    /// Drops the input not read yet.
    pub fn flush_input(&mut self) {
        self.input.clear();
        self.lines_count = 0;
        self.lines_head = 0;
        self.complete = 0;
        self.literal = false;
    }

    /// The bytes of the line being edited.
    fn editing(&self) -> usize {
        self.input.len() - self.complete
    }

    /// How many bytes a read could take now: in canonical mode the first
    /// complete line's (which may be 0, for an end of file), otherwise all
    /// of them. `None` in canonical mode when no line is complete.
    pub fn readable(&self) -> Option<usize> {
        if !self.settings.canonical() {
            return Some(self.input.len());
        }
        (self.lines_count > 0).then(|| usize::from(self.lines[self.lines_head]))
    }

    /// How many bytes are waiting to be read, as FIONREAD counts them.
    pub fn pending(&self) -> usize {
        if self.settings.canonical() { self.complete } else { self.input.len() }
    }

    /// Whether a read would find something now: in canonical mode a
    /// complete line, otherwise at least `min` bytes (VMIN's, at least 1
    /// unless VMIN is 0).
    pub fn ready(&self) -> bool {
        match self.readable() {
            None => false,
            Some(n) if self.settings.canonical() => {
                let _ = n;
                true
            }
            Some(n) => n >= usize::from(self.settings.min_time().0.max(1)),
        }
    }

    /// Takes up to `max` bytes of input through `sink` (as
    /// [`Ring::read_with`]): in canonical mode at most the first complete
    /// line, and an end of file as a read of 0.
    pub fn read_with<E>(
        &mut self,
        max: usize,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<usize, E> {
        if !self.settings.canonical() {
            let n = self.input.read_with(max, sink)?;
            self.complete = self.complete.saturating_sub(n).min(self.input.len());
            return Ok(n);
        }
        let Some(line) = self.readable() else { return Ok(0) };
        let n = self.input.read_with(max.min(line), sink)?;
        self.complete -= n;
        let left = line - n;
        if left == 0 {
            self.lines_head = (self.lines_head + 1) % LINES;
            self.lines_count -= 1;
        } else {
            self.lines[self.lines_head] = left as u16;
        }
        Ok(n)
    }

    /// Completes the line being edited.
    fn end_line(&mut self) {
        let len = self.editing();
        let at = (self.lines_head + self.lines_count) % LINES;
        self.lines[at] = len as u16;
        self.lines_count += 1;
        self.complete += len;
        self.edit_column = self.column;
    }

    /// Takes a byte typed at the terminal. Echoes through `echo` (already
    /// processed for output), and returns the signal its foreground process
    /// group is to get, if the byte was a signal character. Returns
    /// whether readers may have something new, as the second value.
    pub fn receive(&mut self, byte: u8, echo: &mut dyn FnMut(&[u8])) -> (Option<u32>, bool) {
        let s = self.settings;
        let mut byte = byte;
        if s.input(ISTRIP) {
            byte &= 0x7f;
        }
        let literal = core::mem::take(&mut self.literal);
        if !literal {
            if s.input(IXON) {
                if s.is(byte, VSTOP) {
                    self.output_stopped = true;
                    return (None, false);
                }
                if s.is(byte, VSTART) {
                    self.output_stopped = false;
                    return (None, false);
                }
                if s.input(IXANY) {
                    self.output_stopped = false;
                }
            }
            if s.local(ISIG) {
                let signal = if s.is(byte, VINTR) {
                    Some(SIGINT)
                } else if s.is(byte, VQUIT) {
                    Some(SIGQUIT)
                } else if s.is(byte, VSUSP) {
                    Some(SIGTSTP)
                } else {
                    None
                };
                if let Some(signal) = signal {
                    if !s.local(NOFLSH) {
                        self.flush_input();
                    }
                    if s.local(ECHO) {
                        self.echo_char(byte, echo);
                    }
                    return (Some(signal), false);
                }
            }
            if byte == b'\r' {
                if s.input(IGNCR) {
                    return (None, false);
                }
                if s.input(ICRNL) {
                    byte = b'\n';
                }
            } else if byte == b'\n' && s.input(INLCR) {
                byte = b'\r';
            }
        }
        if !s.canonical() {
            let added = self.input.push_byte(byte);
            if added {
                self.complete = self.input.len();
            }
            if s.local(ECHO) || (byte == b'\n' && s.local(ECHONL)) {
                self.echo_char(byte, echo);
            }
            return (None, added);
        }
        if !literal {
            if s.is(byte, VERASE) {
                self.erase(false, echo);
                return (None, false);
            }
            if s.local(IEXTEN) && s.is(byte, VWERASE) {
                self.erase(true, echo);
                return (None, false);
            }
            if s.is(byte, VKILL) {
                self.kill(echo);
                return (None, false);
            }
            if s.local(IEXTEN) && s.is(byte, VLNEXT) {
                self.literal = true;
                if s.local(ECHO) && s.local(ECHOCTL) {
                    self.output_echo(b"^\x08", echo);
                }
                return (None, false);
            }
            if s.local(IEXTEN) && s.is(byte, VREPRINT) {
                self.reprint(byte, echo);
                return (None, false);
            }
            if s.is(byte, VEOF) {
                if self.lines_count == LINES {
                    return (None, false);
                }
                self.end_line();
                return (None, true);
            }
        }
        let ends = byte == b'\n' || (!literal && (s.is(byte, VEOL) || s.is(byte, VEOL2)));
        // A line keeps room for its end, so that a full line can still be
        // finished; with every line slot used, input waits to be read.
        let room = self.input.free();
        if self.lines_count == LINES || room == 0 || (!ends && room == 1) {
            return (None, false);
        }
        self.input.push_byte(byte);
        if s.local(ECHO) || (byte == b'\n' && s.local(ECHONL)) {
            self.echo_char(byte, echo);
        }
        if ends {
            self.end_line();
            return (None, true);
        }
        (None, false)
    }

    /// Echoes `byte` as typed: a control character as `^X` with ECHOCTL.
    fn echo_char(&mut self, byte: u8, echo: &mut dyn FnMut(&[u8])) {
        if self.settings.local(ECHOCTL) && is_control(byte) {
            self.output_echo(&[b'^', byte ^ 0x40], echo);
        } else {
            self.output_echo(&[byte], echo);
        }
    }

    /// Echoes `bytes` through output processing.
    fn output_echo(&mut self, bytes: &[u8], echo: &mut dyn FnMut(&[u8])) {
        let mut done = 0;
        while done < bytes.len() {
            done += self.output(&bytes[done..], usize::MAX, echo);
        }
    }

    /// The width on screen of the line being edited, from its start
    /// column, without its last `drop` bytes.
    fn width_without(&self, drop: usize) -> usize {
        let mut column = self.edit_column;
        let start = self.complete;
        let end = self.input.len() - drop;
        for i in start..end {
            let byte = self.input.get(i).unwrap_or(0);
            column = match byte {
                b'\t' => (column | 7) + 1,
                _ if is_control(byte) && self.settings.local(ECHOCTL) => column + 2,
                _ if is_control(byte) => column,
                0x80..=0xbf if self.settings.input(IUTF8) => column,
                _ => column + 1,
            };
        }
        column
    }

    /// Erases the last character (or with `word`, the last word and the
    /// blanks after it) of the line being edited.
    fn erase(&mut self, word: bool, echo: &mut dyn FnMut(&[u8])) {
        let mut seen_word = false;
        while self.editing() > 0 {
            let last = self.input.get(self.input.len() - 1).unwrap_or(0);
            if word {
                let blank = last == b' ' || last == b'\t';
                if blank && seen_word {
                    break;
                }
                seen_word |= !blank;
            }
            // A UTF-8 character goes whole.
            let mut take = 1;
            if self.settings.input(IUTF8) && last & 0xc0 == 0x80 {
                while take < self.editing()
                    && self.input.get(self.input.len() - 1 - take).is_some_and(|b| b & 0xc0 == 0x80)
                {
                    take += 1;
                }
                if take < self.editing() {
                    take += 1;
                }
            }
            let before = self.width_without(0);
            let after = self.width_without(take);
            for _ in 0..take {
                self.input.pop_back();
            }
            if self.settings.local(ECHO) && self.settings.local(ECHOE) {
                for _ in 0..before.saturating_sub(after) {
                    if last == b'\t' {
                        self.output_echo(b"\x08", echo);
                    } else {
                        self.output_echo(b"\x08 \x08", echo);
                    }
                }
            } else if self.settings.local(ECHO) {
                let erase = self.settings.cc[VERASE as usize];
                self.echo_char(erase, echo);
            }
            if !word {
                break;
            }
        }
    }

    /// Erases the line being edited.
    fn kill(&mut self, echo: &mut dyn FnMut(&[u8])) {
        let s = self.settings;
        if s.local(ECHO) && s.local(ECHOK) && s.local(ECHOKE) && s.local(ECHOE) {
            while self.editing() > 0 {
                self.erase(false, echo);
            }
            return;
        }
        while self.editing() > 0 {
            self.input.pop_back();
        }
        if s.local(ECHO) {
            let kill = s.cc[VKILL as usize];
            self.echo_char(kill, echo);
            if s.local(ECHOK) {
                self.output_echo(b"\n", echo);
            }
        }
        self.edit_column = self.column;
    }

    /// Echoes the REPRINT character, a new line and the line being edited.
    fn reprint(&mut self, byte: u8, echo: &mut dyn FnMut(&[u8])) {
        if !self.settings.local(ECHO) {
            return;
        }
        self.echo_char(byte, echo);
        self.output_echo(b"\n", echo);
        self.edit_column = self.column;
        for i in self.complete..self.input.len() {
            let byte = self.input.get(i).unwrap_or(0);
            self.echo_char(byte, echo);
        }
    }

    /// Processes what a program writes for the screen (OPOST: NL to CR NL
    /// with ONLCR, CR to NL with OCRNL), giving `emit` the result in runs,
    /// at most `room` bytes of it. Returns how many bytes of `data` it
    /// took: fewer when `room` ran out.
    pub fn output(&mut self, data: &[u8], room: usize, emit: &mut dyn FnMut(&[u8])) -> usize {
        let post = self.settings.oflag & OPOST != 0;
        let onlcr = post && self.settings.oflag & ONLCR != 0;
        let ocrnl = post && self.settings.oflag & OCRNL != 0;
        let mut taken = 0;
        let mut left = room;
        let mut run_start = 0;
        while taken < data.len() {
            let byte = data[taken];
            let special = (onlcr && byte == b'\n') || (ocrnl && byte == b'\r');
            if special {
                let need = if byte == b'\n' { 2 } else { 1 };
                // The plain run before it, then it.
                if taken > run_start {
                    emit(&data[run_start..taken]);
                }
                if left < need {
                    return taken;
                }
                emit(if byte == b'\n' { b"\r\n" } else { b"\n" });
                left -= need;
                self.column = 0;
                taken += 1;
                run_start = taken;
                continue;
            }
            if left == 0 {
                break;
            }
            self.column = match byte {
                b'\r' | b'\n' => 0,
                b'\t' => (self.column | 7) + 1,
                0x08 => self.column.saturating_sub(1),
                _ if is_control(byte) => self.column,
                0x80..=0xbf => self.column,
                _ => self.column + 1,
            };
            left -= 1;
            taken += 1;
        }
        if taken > run_start {
            emit(&data[run_start..taken]);
        }
        taken
    }
}

/// Whether `byte` is a control character echoed as `^X`: below space or
/// DEL, but for tab and NL.
fn is_control(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t' && byte != b'\n') || byte == 0x7f
}
