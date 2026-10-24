// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Redirections (XCU 2.7).
//!
//! For a command the shell runs itself (a builtin, a function, a compound
//! command), each descriptor a redirection changes is first copied away,
//! and put back afterwards. The copies sit at descriptor 10 or above;
//! should a later redirection name one of them (`exec 10>file`), it moves
//! out of the way first.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{HereDoc, RedirFd, RedirOp, RedirTarget, Redirect, Word, WordPart};
use crate::shell::{Flow, Shell};
use crate::{sys, text};

/// What to put back after a command's redirections: each changed
/// descriptor, and the slot of its saved copy (none if it was closed).
#[derive(Debug, Default)]
pub struct Saved {
    fds: Vec<(i32, Option<usize>)>,
}

/// Why redirections failed: a message was printed, and the command does
/// not run; a fatal expansion error carries its flow.
pub type RedirError = Option<Flow>;

impl Shell {
    /// Applies `redirects`; with `save`, so that [`Shell::restore`] can
    /// undo them.
    pub fn redirect(&mut self, redirects: &[Redirect], save: bool) -> Result<Saved, RedirError> {
        let mut saved = Saved::default();
        for r in redirects {
            if let Err(error) = self.redirect_one(r, save, &mut saved) {
                self.restore(saved);
                return Err(error);
            }
        }
        Ok(saved)
    }

    /// `redirects` with their words and here-documents expanded, to be
    /// taken as they are: POSIX's order, in which a command's
    /// redirections are expanded before its assignments.
    pub fn expand_redirects(&mut self, redirects: &[Redirect]) -> Result<Vec<Redirect>, Flow> {
        let literal = |text: String| Word { parts: vec![WordPart::Quoted(text)] };
        let mut expanded = Vec::with_capacity(redirects.len());
        for r in redirects {
            let target = match &r.target {
                RedirTarget::Word(word) => RedirTarget::Word(literal(self.expand_string(word)?)),
                RedirTarget::HereDoc(doc) if doc.borrow().expand => {
                    let body = self.expand_quoted(&doc.borrow().body)?;
                    RedirTarget::HereDoc(Rc::new(RefCell::new(HereDoc {
                        body: literal(body),
                        expand: false,
                    })))
                }
                RedirTarget::HereDoc(_) => r.target.clone(),
            };
            expanded.push(Redirect { fd: r.fd.clone(), op: r.op, target });
        }
        Ok(expanded)
    }

    /// Puts back what [`Shell::redirect`] changed.
    pub fn restore(&mut self, saved: Saved) {
        for (fd, slot) in saved.fds.into_iter().rev() {
            match slot.and_then(|s| self.held_fd(s)) {
                Some(copy) => {
                    let _ = sys::dup2(copy, fd);
                    sys::close(copy);
                }
                None => sys::close(fd),
            }
            if let Some(slot) = slot {
                self.release(slot);
            }
        }
    }

    fn held_fd(&self, slot: usize) -> Option<i32> {
        self.held.get(slot).copied().flatten()
    }

    fn release(&mut self, slot: usize) {
        if let Some(entry) = self.held.get_mut(slot) {
            *entry = None;
        }
        while self.held.last() == Some(&None) {
            self.held.pop();
        }
    }

    /// Keeps a copy of `fd` in a slot.
    fn hold(&mut self, fd: i32) -> std::io::Result<usize> {
        let copy = sys::dup_high(fd)?;
        let slot = match self.held.iter().position(Option::is_none) {
            Some(free) => {
                self.held[free] = Some(copy);
                free
            }
            None => {
                self.held.push(Some(copy));
                self.held.len() - 1
            }
        };
        Ok(slot)
    }

    /// Moves a saved copy that sits on `fd` somewhere else, so that `fd`
    /// can be redirected.
    pub fn make_room(&mut self, fd: i32) {
        if fd < sys::HIGH_FD {
            return;
        }
        if self.tty == Some(fd)
            && let Ok(moved) = sys::dup_high(fd)
        {
            sys::close(fd);
            self.tty = Some(moved);
        }
        for entry in self.held.iter_mut() {
            if *entry == Some(fd)
                && let Ok(moved) = sys::dup_high(fd)
            {
                sys::close(fd);
                *entry = Some(moved);
            }
        }
    }

    /// Notes that `fd` is about to change.
    fn save_fd(&mut self, fd: i32, save: bool, saved: &mut Saved) -> Result<(), RedirError> {
        self.make_room(fd);
        if !save || saved.fds.iter().any(|(f, _)| *f == fd) {
            return Ok(());
        }
        let slot = if sys::is_open(fd) {
            match self.hold(fd) {
                Ok(slot) => Some(slot),
                Err(error) => {
                    self.error(&format!(
                        "redirection error: cannot duplicate fd: {}",
                        sys::describe(&error)
                    ));
                    return Err(None);
                }
            }
        } else {
            None
        };
        saved.fds.push((fd, slot));
        Ok(())
    }

    fn redirect_one(
        &mut self,
        r: &Redirect,
        save: bool,
        saved: &mut Saved,
    ) -> Result<(), RedirError> {
        let default_fd = match r.op {
            RedirOp::Read
            | RedirOp::ReadWrite
            | RedirOp::DupRead
            | RedirOp::HereDoc
            | RedirOp::HereString => 0,
            _ => 1,
        };
        // `{name}>file`: a new descriptor, put in `name`.
        let mut named = None;
        let fd = match &r.fd {
            Some(RedirFd::Number(n)) => *n,
            Some(RedirFd::Var(name)) => {
                named = Some(name.clone());
                -1
            }
            None => default_fd,
        };
        let restricted = self.opts.restricted
            && matches!(
                r.op,
                RedirOp::Write
                    | RedirOp::Clobber
                    | RedirOp::Append
                    | RedirOp::ReadWrite
                    | RedirOp::WriteBoth
                    | RedirOp::AppendBoth
            );
        let source = match r.op {
            RedirOp::HereDoc => {
                let RedirTarget::HereDoc(doc) = &r.target else { return Err(None) };
                let doc = doc.borrow().clone();
                let body = if doc.expand {
                    self.expand_quoted(&doc.body).map_err(Some)?
                } else {
                    doc.body
                        .as_literal()
                        .map(str::to_string)
                        .unwrap_or_else(|| crate::show::word_text(&doc.body))
                };
                let body = match doc.body.parts.as_slice() {
                    [crate::ast::WordPart::Quoted(text)] if !doc.expand => text.clone(),
                    _ => body,
                };
                Some(self.here_document(&text::encode(&body))?)
            }
            RedirOp::HereString => {
                let RedirTarget::Word(word) = &r.target else { return Err(None) };
                let mut body = self.expand_string(word).map_err(Some)?;
                body.push('\n');
                Some(self.here_document(&text::encode(&body))?)
            }
            _ => None,
        };
        let RedirTarget::Word(word) = &r.target else {
            return self.install(source.unwrap_or(-1), fd, named, save, saved);
        };
        if let Some(source) = source {
            return self.install(source, fd, named, save, saved);
        }
        let target = self.expand_string(word).map_err(Some)?;
        match r.op {
            RedirOp::DupRead | RedirOp::DupWrite => {
                if target == "-" {
                    let fd = match &named {
                        Some(name) => match self.get(name).and_then(|v| v.parse().ok()) {
                            Some(n) => n,
                            None => return Ok(()),
                        },
                        None => fd,
                    };
                    self.save_fd(fd, save, saved)?;
                    sys::close(fd);
                    return Ok(());
                }
                let (digits, close) = match target.strip_suffix('-') {
                    Some(digits) => (digits, true),
                    None => (target.as_str(), false),
                };
                if let Ok(from) = digits.parse::<i32>() {
                    if !sys::is_open(from) || self.held.contains(&Some(from)) {
                        self.error(&format!("{from}: Bad file descriptor"));
                        return Err(None);
                    }
                    if named.is_none() && from == fd {
                        return Ok(());
                    }
                    let copy = match sys::dup_high(from) {
                        Ok(copy) => copy,
                        Err(error) => {
                            self.error(&format!("{from}: {}", sys::describe(&error)));
                            return Err(None);
                        }
                    };
                    self.install(copy, fd, named, save, saved)?;
                    if close {
                        self.save_fd(from, save, saved)?;
                        sys::close(from);
                    }
                    return Ok(());
                }
                if r.op == RedirOp::DupWrite && r.fd.is_none() {
                    // `>&file` is `&>file`.
                    if restricted {
                        self.error(&format!("{target}: restricted: cannot redirect output"));
                        return Err(None);
                    }
                    let opened = self.open_target(&target, RedirOp::Write)?;
                    let copy = sys::dup_high(opened).ok();
                    self.install(opened, 1, None, save, saved)?;
                    if let Some(copy) = copy {
                        self.install(copy, 2, None, save, saved)?;
                    }
                    return Ok(());
                }
                self.error(&format!("{target}: ambiguous redirect"));
                Err(None)
            }
            op => {
                if restricted {
                    self.error(&format!("{target}: restricted: cannot redirect output"));
                    return Err(None);
                }
                if target.is_empty()
                    && !matches!(
                        word.parts.as_slice(),
                        [crate::ast::WordPart::Double(_)] | [crate::ast::WordPart::Quoted(_)]
                    )
                {
                    self.error(&format!("{}: ambiguous redirect", crate::show::word_text(word)));
                    return Err(None);
                }
                let opened = self.open_target(&target, op)?;
                if matches!(op, RedirOp::WriteBoth | RedirOp::AppendBoth) {
                    let copy = sys::dup_high(opened).ok();
                    self.install(opened, 1, None, save, saved)?;
                    if let Some(copy) = copy {
                        self.install(copy, 2, None, save, saved)?;
                    }
                    return Ok(());
                }
                self.install(opened, fd, named, save, saved)
            }
        }
    }

    /// Opens a redirection's file.
    fn open_target(&mut self, path: &str, op: RedirOp) -> Result<i32, RedirError> {
        let mode = 0o666;
        let flags = match op {
            RedirOp::Read => libc::O_RDONLY,
            RedirOp::ReadWrite => libc::O_RDWR | libc::O_CREAT,
            RedirOp::Append | RedirOp::AppendBoth => {
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND
            }
            RedirOp::Write | RedirOp::WriteBoth if self.opts.noclobber => {
                // Only a regular file is protected; /dev/null is not.
                let regular = std::fs::metadata(text::to_os(path)).is_ok_and(|m| m.is_file());
                if regular {
                    self.error(&format!("{path}: cannot overwrite existing file"));
                    return Err(None);
                }
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC
            }
            _ => libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
        };
        match sys::open(path, flags, mode) {
            Ok(fd) => Ok(fd),
            Err(error) => {
                self.error(&format!("{path}: {}", sys::describe(&error)));
                Err(None)
            }
        }
    }

    /// Puts descriptor `source` at `fd` (or for `{name}`, at a new number
    /// stored in `name`), and closes `source`.
    fn install(
        &mut self,
        source: i32,
        fd: i32,
        named: Option<String>,
        save: bool,
        saved: &mut Saved,
    ) -> Result<(), RedirError> {
        if source < 0 {
            return Err(None);
        }
        if let Some(name) = named {
            // A new descriptor, not closed on exec, that outlives the
            // command.
            // SAFETY: F_DUPFD only acts on the descriptor table.
            let new = unsafe { libc::fcntl(source, libc::F_DUPFD, sys::HIGH_FD) };
            sys::close(source);
            if new < 0 {
                self.error("cannot allocate a file descriptor");
                return Err(None);
            }
            if let Err(message) = self.assign(&name, new.to_string(), false) {
                sys::close(new);
                self.error(&message);
                return Err(None);
            }
            self.named_fds.push(new);
            return Ok(());
        }
        self.save_fd(fd, save, saved)?;
        if let Err(error) = sys::dup2(source, fd) {
            sys::close(source);
            self.error(&format!("{fd}: {}", sys::describe(&error)));
            return Err(None);
        }
        if source != fd {
            sys::close(source);
        }
        Ok(())
    }

    /// A descriptor to read `body` from: a pipe filled at once when it
    /// fits, or one a child fills.
    fn here_document(&mut self, body: &[u8]) -> Result<i32, RedirError> {
        let (read, write) = match sys::pipe() {
            Ok(fds) => fds,
            Err(error) => {
                self.error(&format!("cannot make a here-document: {}", sys::describe(&error)));
                return Err(None);
            }
        };
        if body.len() <= 65536 {
            let _ = sys::write_all(write, body);
            sys::close(write);
            return Ok(read);
        }
        match sys::fork() {
            Ok(0) => {
                sys::close(read);
                crate::trap::set(libc::SIGPIPE, crate::trap::Disposition::Default);
                let _ = sys::write_all(write, body);
                sys::exit_now(0);
            }
            Ok(_) => {
                sys::close(write);
                Ok(read)
            }
            Err(error) => {
                sys::close(read);
                sys::close(write);
                self.error(&format!("cannot make a here-document: {}", sys::describe(&error)));
                Err(None)
            }
        }
    }
}
