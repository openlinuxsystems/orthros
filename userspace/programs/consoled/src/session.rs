// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A virtual console: its terminal, and the session on it, a login shell
//! on a pseudo-terminal of the console's size, started again when it ends,
//! as getty(8) starts login(1) again on Linux.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crate::term::Terminal;

/// A session that ends sooner than this after it started ended at once.
const QUICK: Duration = Duration::from_secs(2);
/// After this many sessions in a row ended at once, the console waits
/// before it starts another.
const QUICK_MAX: u32 = 5;
/// How long it waits then.
const PAUSE: Duration = Duration::from_secs(10);

/// What each console's session runs.
pub struct Shell {
    /// The program's path.
    pub path: String,
    /// The environment it gets, as `NAME=value`.
    pub env: Vec<String>,
    /// The text shown before it starts: /system/etc/issue's.
    pub issue: String,
}

/// One virtual console.
pub struct Console {
    /// Its number, from 1: the N of ttyN and Alt+FN.
    pub number: usize,
    /// What it shows.
    pub term: Terminal,
    master: Option<OwnedFd>,
    /// The session's shell, while it runs.
    child: Option<libc::pid_t>,
    /// The master has hung up: every descriptor of its terminal is
    /// closed, though the shell has not been reaped yet.
    hung_up: bool,
    started: Instant,
    quick_exits: u32,
    /// When to start the next session, while it waits after quick exits.
    pub retry_at: Option<Instant>,
}

/// An argument or environment string as C wants it; one with a NUL in it
/// becomes empty.
fn c_string(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

impl Console {
    /// Console `number`, of `cols` by `rows` cells, with no session yet.
    pub fn new(number: usize, cols: usize, rows: usize) -> Console {
        Console {
            number,
            term: Terminal::new(cols, rows),
            master: None,
            child: None,
            hung_up: false,
            started: Instant::now(),
            quick_exits: 0,
            retry_at: None,
        }
    }

    /// Whether a session runs or waits to run.
    pub fn has_session(&self) -> bool {
        self.child.is_some() || self.retry_at.is_some()
    }

    /// The master's descriptor, while it can be read.
    pub fn master_fd(&self) -> Option<RawFd> {
        self.master.as_ref().filter(|_| !self.hung_up).map(AsRawFd::as_raw_fd)
    }

    /// The session's process.
    pub fn child(&self) -> Option<libc::pid_t> {
        self.child
    }

    /// Starts a session: shows the issue text, then runs the shell on a
    /// new pseudo-terminal as its controlling terminal.
    pub fn start(&mut self, shell: &Shell) -> io::Result<()> {
        self.retry_at = None;
        self.term.reset();
        let issue = expand_issue(&shell.issue, self.number);
        self.term.feed(issue.replace('\n', "\r\n").as_bytes());
        let size = libc::winsize {
            ws_row: self.term.rows() as u16,
            ws_col: self.term.cols() as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty writes the two descriptors; the name may be null.
        let opened = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size)
        };
        if opened != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty made both descriptors, which are ours alone.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        set_flags(master.as_raw_fd());
        // The login shell's name starts with a dash (sh(1)).
        let name = shell.path.rsplit('/').next().unwrap_or("sh");
        let argv_strings = [c_string(&shell.path), c_string(&format!("-{name}"))];
        let argv = [argv_strings[1].as_ptr(), std::ptr::null()];
        let env_strings: Vec<CString> = shell.env.iter().map(|e| c_string(e)).collect();
        let mut envp: Vec<*const libc::c_char> = env_strings.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        // SAFETY: consoled has one thread; the child only makes the
        // terminal its own and execs, with what was built before the fork.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: async-signal-safe calls on the child's own copies of
            // the descriptors and the strings built before the fork.
            unsafe {
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                libc::close(master.as_raw_fd());
                libc::login_tty(slave.as_raw_fd());
                libc::execve(argv_strings[0].as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        drop(slave);
        self.master = Some(master);
        self.child = Some(pid);
        self.hung_up = false;
        self.started = Instant::now();
        Ok(())
    }

    /// Takes in what the session wrote. Returns false once the terminal's
    /// other side has closed.
    pub fn read(&mut self) -> bool {
        let Some(fd) = self.master_fd() else { return false };
        let mut buffer = [0u8; 16384];
        // A burst of output is drawn once: up to four buffers a turn.
        for _ in 0..4 {
            // SAFETY: reads into our buffer.
            let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n > 0 {
                self.term.feed(&buffer[..n as usize]);
                continue;
            }
            let error = io::Error::last_os_error();
            if n < 0 && error.kind() == io::ErrorKind::WouldBlock {
                break;
            }
            if n < 0 && error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // EOF, or EIO: no descriptor of the terminal is open.
            self.hung_up = true;
            return false;
        }
        let replies = self.term.take_replies();
        if !replies.is_empty() {
            self.write(&replies);
        }
        true
    }

    /// Sends `bytes` to the session as typed. What the terminal has no room
    /// for is dropped, as a program that does not read its input loses
    /// keys typed at it.
    pub fn write(&mut self, bytes: &[u8]) {
        let Some(fd) = self.master_fd() else { return };
        let mut at = 0;
        while at < bytes.len() {
            // SAFETY: writes from our buffer.
            let n = unsafe { libc::write(fd, bytes[at..].as_ptr().cast(), bytes.len() - at) };
            if n > 0 {
                at += n as usize;
            } else if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            } else {
                break;
            }
        }
    }

    /// The session's shell ended with wait status `status`: the terminal
    /// goes (hanging up what still uses it), and a new session starts, at
    /// once unless sessions keep ending at once. Returns a line for the
    /// log when it waits.
    pub fn ended(&mut self, status: i32, shell: &Shell) -> Option<String> {
        self.child = None;
        self.master = None;
        self.hung_up = false;
        if self.started.elapsed() < QUICK {
            self.quick_exits += 1;
        } else {
            self.quick_exits = 0;
        }
        if self.quick_exits >= QUICK_MAX {
            self.quick_exits = 0;
            self.retry_at = Some(Instant::now() + PAUSE);
            let message = format!(
                "{} ended at once {QUICK_MAX} times on tty{} (last status {status:#x}); \
                 starting it again in {} seconds",
                shell.path,
                self.number,
                PAUSE.as_secs()
            );
            self.term.feed(format!("\r\nconsoled: {message}\r\n").as_bytes());
            return Some(message);
        }
        match self.start(shell) {
            Ok(()) => None,
            Err(err) => {
                self.retry_at = Some(Instant::now() + PAUSE);
                Some(format!("cannot start {} on tty{}: {err}", shell.path, self.number))
            }
        }
    }
}

/// Makes the master non-blocking and closed on exec.
fn set_flags(fd: RawFd) {
    // SAFETY: fcntl on our own descriptor.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// /system/etc/issue's text for console `number`, with agetty(8)'s
/// escapes: `\l` the terminal's name, `\n` the host's name, `\s` the
/// system's, `\r` its release, `\v` its version, `\m` the machine, `\\` a
/// backslash.
pub fn expand_issue(issue: &str, number: usize) -> String {
    // SAFETY: uname fills the structure.
    let mut names: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let named = unsafe { libc::uname(&mut names) } == 0;
    let field = |field: &[libc::c_char]| -> String {
        if !named {
            return String::new();
        }
        let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let mut out = String::new();
    let mut chars = issue.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('l') => out.push_str(&format!("tty{number}")),
            Some('n') => out.push_str(&field(&names.nodename)),
            Some('s') => out.push_str(&field(&names.sysname)),
            Some('r') => out.push_str(&field(&names.release)),
            Some('v') => out.push_str(&field(&names.version)),
            Some('m') => out.push_str(&field(&names.machine)),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_the_issue_escapes() {
        assert_eq!(expand_issue("on \\l, \\\\ \\q", 3), "on tty3, \\ q");
        assert!(!expand_issue("\\s", 1).contains('\\'));
    }
}
