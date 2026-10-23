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

    /// Puts a file's offset back to the end of what was taken, so that a
    /// command that reads the same file starts there.
    pub fn sync(&mut self) {
        let ahead = self.buf.len() - self.pos;
        if self.regular && ahead > 0 && sys::lseek(self.fd, -(ahead as i64), libc::SEEK_CUR).is_ok()
        {
            self.buf.clear();
            self.pos = 0;
        }
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
        loop {
            parser.set_options(self.parse_options());
            match parser.next_command() {
                Ok(Some(command)) => {
                    if self.opts.noexec {
                        continue;
                    }
                    lines.sync();
                    status = self.run_complete(&command)?;
                    self.close_procsubs();
                    self.notify_background();
                    parser.compact();
                }
                Ok(None) | Err(parse::Error::Incomplete(_)) if !at_end => match lines.next_line() {
                    Some(bytes) => {
                        let line = text::decode(&bytes);
                        if self.opts.verbose {
                            let _ = sys::write_all(2, &bytes);
                        }
                        parser.push_text(&line);
                    }
                    None => at_end = true,
                },
                Ok(None) => return Ok(status),
                Err(error) => {
                    self.lineno = error.line().unwrap_or(parser.line());
                    let message = match &error {
                        parse::Error::Incomplete(message) => {
                            format!("syntax error: unexpected end of file ({message})")
                        }
                        parse::Error::Syntax { message, .. } => format!("syntax error: {message}"),
                    };
                    self.error(&message);
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
    fn a_file_is_seeked_back_to_the_line_end() {
        let path = std::env::temp_dir().join(format!("nukesh-lines-{}", std::process::id()));
        std::fs::write(&path, b"first\nsecond\n").unwrap();
        let fd = sys::open(&path.to_string_lossy(), libc::O_RDONLY, 0).unwrap();
        let mut lines = Lines::new(fd);
        assert_eq!(lines.next_line().as_deref(), Some(&b"first\n"[..]));
        lines.sync();
        let mut rest = [0u8; 16];
        let n = sys::read(fd, &mut rest).unwrap();
        assert_eq!(&rest[..n], b"second\n");
        sys::close(fd);
        let _ = std::fs::remove_file(path);
    }
}
