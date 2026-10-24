// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reading a script from a file or from standard input, a line at a time,
//! so that each command runs before the lines after it are read.
//!
//! A command can read the same input as the shell: `read` in a script
//! that comes on a pipe, or `cat` in `sh <<EOF`. It gets what follows the
//! command, as in other shells, because the shell never takes more than
//! it has run: a pipe or a terminal is read a byte at a time, and a file
//! in blocks, with the offset put back to the end of the command before
//! the command runs.
//!
//! A command that is not complete at the end of its line is parsed again
//! from its start when more comes. From a file, twice as many lines come
//! each time, so that a function of thousands of lines is parsed a few
//! times, not once per line; what came after the command is read again.

use std::rc::Rc;

use crate::parse::{self, Parser};
use crate::shell::{Flow, Res, Shell};
use crate::{sys, text};

/// How much of a file is read at once.
const BLOCK: usize = 4096;

/// Lines from a descriptor.
pub struct Lines {
    fd: i32,
    /// A file, which can be read ahead and seeked back.
    regular: bool,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl Lines {
    pub fn new(fd: i32) -> Lines {
        Lines { fd, regular: sys::is_regular(fd), buf: Vec::new(), pos: 0, done: false }
    }

    /// The next line, with its newline if it has one: `None` at the end.
    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            if let Some(&byte) = self.buf.get(self.pos) {
                self.pos += 1;
                line.push(byte);
                if byte == b'\n' {
                    return Some(line);
                }
                continue;
            }
            if self.done || !self.fill() {
                self.done = true;
                return (!line.is_empty()).then_some(line);
            }
        }
    }

    /// Reads more: a block from a file, a byte from anything else. False at
    /// the end, or on an error.
    fn fill(&mut self) -> bool {
        let size = if self.regular { BLOCK } else { 1 };
        self.buf.resize(size, 0);
        self.pos = 0;
        match sys::read(self.fd, &mut self.buf) {
            Ok(0) | Err(_) => {
                self.buf.clear();
                false
            }
            Ok(n) => {
                self.buf.truncate(n);
                true
            }
        }
    }

    /// Puts a file's offset back to the end of what was taken, less the
    /// `unread` bytes at its end that the caller gives back, so that a
    /// command that reads the same file starts there. False if a file's
    /// offset could not be put back.
    pub fn sync(&mut self, unread: usize) -> bool {
        let back = self.buf.len() - self.pos + unread;
        if !self.regular || back == 0 {
            return unread == 0;
        }
        if sys::lseek(self.fd, -(back as i64), libc::SEEK_CUR).is_err() {
            return false;
        }
        self.buf.clear();
        self.pos = 0;
        self.done = false;
        true
    }
}

impl Shell {
    /// Runs the commands read from `fd`: a script, or standard input. An
    /// error that ends a non-interactive shell comes back as `Err`.
    pub fn run_fd(&mut self, fd: i32, file: Rc<str>) -> Res {
        let mut lines = Lines::new(fd);
        let mut parser = Parser::new("", file, 1, self.parse_options());
        let mut status = 0;
        let mut at_end = false;
        // The lines to add before the next try at a command.
        let mut batch: usize = 1;
        loop {
            parser.set_options(self.parse_options());
            let next = parser.next_command();
            self.parse_warnings(&mut parser);
            match next {
                Ok(Some(command)) => {
                    batch = 1;
                    if self.opts.noexec {
                        continue;
                    }
                    // What was read past the command is read again after it.
                    match parser.take_rest() {
                        Some(rest) if lines.sync(text::encode(&rest).len()) => at_end = false,
                        Some(rest) => {
                            parser.push_text(&rest);
                            if at_end {
                                parser.end_text();
                            }
                        }
                        None => {
                            lines.sync(0);
                        }
                    }
                    self.report_line = parser.command_end();
                    // Background jobs that ended are news before it.
                    self.notify_background();
                    status = match self.run_complete(&command) {
                        // An interactive shell reading a file (`sh -i
                        // file`) goes on after an error, as at its prompt.
                        Err(Flow::Fatal(code)) if self.interactive => {
                            self.status = code;
                            code
                        }
                        // The next command runs, as in bash.
                        Err(Flow::Discard(code)) => {
                            self.status = code;
                            code
                        }
                        other => other?,
                    };
                    self.close_procsubs();
                    parser.compact();
                }
                Ok(None) | Err(parse::Error::Incomplete(_)) if !at_end => {
                    // `set -v` shows each line once, as it is read.
                    let ahead = lines.regular && !self.opts.verbose;
                    let count = if ahead { batch } else { 1 };
                    if ahead {
                        batch = batch.saturating_mul(2);
                    }
                    for _ in 0..count {
                        let Some(bytes) = lines.next_line() else {
                            at_end = true;
                            parser.end_text();
                            break;
                        };
                        if self.opts.verbose {
                            let _ = sys::write_all(2, &bytes);
                        }
                        parser.push_text(&text::decode(&bytes));
                    }
                }
                Ok(None) => return Ok(status),
                Err(error) => {
                    self.syntax_error(&error, None, parser.end_line());
                    self.syntax_failed = true;
                    return Err(Flow::Fatal(2));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_come_with_their_newlines() {
        let (read, write) = sys::pipe().unwrap();
        sys::write_all(write, b"one\ntwo\nlast").unwrap();
        sys::close(write);
        let mut lines = Lines::new(read);
        assert_eq!(lines.next_line().as_deref(), Some(&b"one\n"[..]));
        assert_eq!(lines.next_line().as_deref(), Some(&b"two\n"[..]));
        assert_eq!(lines.next_line().as_deref(), Some(&b"last"[..]));
        assert_eq!(lines.next_line(), None);
        sys::close(read);
    }

    #[test]
    fn what_was_read_past_a_command_is_given_back() {
        let path = std::env::temp_dir().join(format!("nukesh-unread-{}", std::process::id()));
        std::fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        let fd = sys::open(&path.to_string_lossy(), libc::O_RDONLY, 0).unwrap();
        let mut lines = Lines::new(fd);
        assert_eq!(lines.next_line().as_deref(), Some(&b"one\n"[..]));
        assert_eq!(lines.next_line().as_deref(), Some(&b"two\n"[..]));
        // "two\n" was read past the command: it comes again.
        assert!(lines.sync(4));
        assert_eq!(lines.next_line().as_deref(), Some(&b"two\n"[..]));
        sys::close(fd);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_file_is_seeked_back_to_the_line_end() {
        let path = std::env::temp_dir().join(format!("nukesh-lines-{}", std::process::id()));
        std::fs::write(&path, b"first\nsecond\n").unwrap();
        let fd = sys::open(&path.to_string_lossy(), libc::O_RDONLY, 0).unwrap();
        let mut lines = Lines::new(fd);
        assert_eq!(lines.next_line().as_deref(), Some(&b"first\n"[..]));
        assert!(lines.sync(0));
        let mut rest = [0u8; 16];
        let n = sys::read(fd, &mut rest).unwrap();
        assert_eq!(&rest[..n], b"second\n");
        sys::close(fd);
        let _ = std::fs::remove_file(path);
    }
}
