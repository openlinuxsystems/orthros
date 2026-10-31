// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Asking at the terminal: a line, a line not shown, as a password is,
//! and a yes or no. Ctrl+C stops a question, not the program, which puts
//! the terminal's echo back first.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::util;

/// Set by a signal that stops the question.
static STOPPED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOPPED.store(true, Ordering::Relaxed);
}

/// Whether the standard input is a terminal.
pub fn interactive() -> bool {
    // SAFETY: isatty only looks at the descriptor.
    unsafe { libc::isatty(0) == 1 }
}

/// The signals that stop a question.
const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Has SIGINT, SIGTERM and SIGHUP stop the question at hand: without
/// SA_RESTART, the read(2) waiting for it ends with EINTR. Gives what
/// they did before, for `restore_signals`.
fn catch_signals() -> [libc::sigaction; 3] {
    // SAFETY: all zeros is a valid sigaction: numbers, a signal set and
    // a function address.
    let mut before: [libc::sigaction; 3] = unsafe { util::zeroed() };
    for (signal, before) in SIGNALS.iter().zip(&mut before) {
        // SAFETY: as above.
        let mut action: libc::sigaction = unsafe { util::zeroed() };
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as *const () as usize;
        // SAFETY: the handler only stores to an atomic; `before` is a
        // sigaction to fill.
        unsafe { libc::sigaction(*signal, &raw const action, before) };
    }
    before
}

/// Has the signals do again what they did before the question: once it
/// is over, Ctrl+C ends the program as it would have.
fn restore_signals(before: &[libc::sigaction; 3]) {
    for (signal, before) in SIGNALS.iter().zip(before) {
        // SAFETY: `before` is what sigaction gave for this signal.
        unsafe { libc::sigaction(*signal, before, std::ptr::null_mut()) };
    }
}

/// Reads descriptor `fd` a byte at a time into `line` up to its newline,
/// which it leaves out, or its end; stops early when a signal came.
/// Whether a newline or the end came.
fn read_line(fd: RawFd, line: &mut Vec<u8>) -> io::Result<bool> {
    let mut byte = 0u8;
    while !STOPPED.load(Ordering::Relaxed) {
        // SAFETY: `byte` is one writable byte.
        match unsafe { libc::read(fd, (&raw mut byte).cast(), 1) } {
            0 => return Ok(!line.is_empty()),
            1 if byte == b'\n' => return Ok(true),
            1 => line.push(byte),
            _ => match io::Error::last_os_error() {
                err if err.kind() == io::ErrorKind::Interrupted => {}
                err => return Err(err),
            },
        }
    }
    Ok(false)
}

/// What a question got.
pub enum Answer {
    /// A line, without its newline.
    Line(Vec<u8>),
    /// Nothing: the input ended, or a signal stopped the question.
    Stopped,
}

/// Shows `prompt` on the standard error and reads a line from the
/// standard input, with the terminal's echo off unless `shown`.
pub fn ask(prompt: &str, shown: bool) -> io::Result<Answer> {
    ask_on(0, 2, prompt, shown)
}

/// Shows `prompt` on descriptor `out` and reads a line from descriptor
/// `fd`, with the terminal's echo off unless `shown`: [`ask`] on other
/// descriptors, as elevate(8) asks on its terminal whatever its input
/// and output are.
pub fn ask_on(fd: RawFd, out: RawFd, prompt: &str, shown: bool) -> io::Result<Answer> {
    STOPPED.store(false, Ordering::Relaxed);
    let say = |text: &[u8]| {
        // SAFETY: writes from a valid buffer; a short write only loses
        // some of the prompt.
        unsafe { libc::write(out, text.as_ptr().cast(), text.len()) };
    };
    let _ = io::stderr().flush();
    say(prompt.as_bytes());
    let before = catch_signals();
    // SAFETY: all zeros is a valid termios: numbers.
    let mut saved: libc::termios = unsafe { util::zeroed() };
    // SAFETY: tcgetattr fills `saved`.
    let quiet = !shown && unsafe { libc::tcgetattr(fd, &raw mut saved) } == 0;
    if quiet {
        let mut off = saved;
        off.c_lflag &= !libc::ECHO;
        // SAFETY: `off` is a valid termios.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const off) };
    }
    let mut line = Vec::new();
    let read = read_line(fd, &mut line);
    if quiet {
        // SAFETY: `saved` is the termios read above.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const saved) };
        say(b"\n");
    }
    restore_signals(&before);
    match read {
        Ok(true) if !STOPPED.load(Ordering::Relaxed) => Ok(Answer::Line(line)),
        Ok(_) => {
            accounts::wipe(&mut line);
            if shown {
                say(b"\n");
            }
            Ok(Answer::Stopped)
        }
        Err(err) => {
            accounts::wipe(&mut line);
            Err(err)
        }
    }
}

/// Asks `question` until it is answered yes or no; no unless it is.
pub fn confirm(question: &str) -> bool {
    loop {
        match ask(&format!("{question} [y/N] "), true) {
            Ok(Answer::Line(line)) => match line.trim_ascii() {
                b"y" | b"Y" | b"yes" | b"Yes" | b"YES" => return true,
                b"" | b"n" | b"N" | b"no" | b"No" | b"NO" => return false,
                _ => {}
            },
            _ => return false,
        }
    }
}
