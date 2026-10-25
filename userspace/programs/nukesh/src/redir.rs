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
use crate::{show, sys, text};

/// What to put back after a command's redirections: each changed
/// descriptor, and the slot of its saved copy (none if it was closed).
#[derive(Debug, Default)]
pub struct Saved {
    fds: Vec<(i32, Option<usize>)>,
}

/// Why redirections failed: a message was printed, and the command does
/// not run; a fatal expansion error carries its flow.
pub type RedirError = Option<Flow>;

/// How long a command's redirections last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Until the command is done, then undone: a builtin's, a function's,
    /// a compound command's.
    Command,
    /// For good, in the shell: `exec`'s.
    Shell,
    /// In the child that runs a program, which keeps them.
    Child,
}

/// `>&word` and `<&word` as written: whether it ends in an unquoted `-`,
/// a move (`3>&$fd-`), and the word without it.
fn move_word(word: &Word) -> (Word, bool) {
    if word.as_literal() == Some("-") {
        return (word.clone(), false);
    }
    match word.parts.last() {
        Some(WordPart::Lit(last)) if last.ends_with('-') => {
            let mut word = word.clone();
            let rest = &last[..last.len() - 1];
            if rest.is_empty() {
                word.parts.pop();
            } else if let Some(part) = word.parts.last_mut() {
                *part = WordPart::Lit(rest.to_string());
            }
            (word, true)
        }
        _ => (word.clone(), false),
    }
}

/// A descriptor written as a number (`>&7`, `<&07-`), which messages name
/// by its number; a larger one is a word, as in bash.
fn literal_fd(word: &Word) -> Option<i32> {
    let text = word.as_literal()?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Shell {
    /// Applies `redirects`, for `scope`; for a command's, so that
    /// [`Shell::restore`] can undo them.
    pub fn redirect(&mut self, redirects: &[Redirect], scope: Scope) -> Result<Saved, RedirError> {
        let mut saved = Saved::default();
        for r in redirects {
            if let Err(error) = self.redirect_one(r, scope, &mut saved) {
                self.restore(saved);
                return Err(error);
            }
        }
        Ok(saved)
    }

    /// What a redirection's word comes to (see [`Shell::redirect_word`]):
    /// for `>&` and `<&`, without the `-` of a move; for `<<<`, the whole
    /// word, unsplit.
    fn target_text(&mut self, op: RedirOp, word: &Word) -> Result<Option<String>, Flow> {
        match op {
            RedirOp::DupRead | RedirOp::DupWrite => self.redirect_word(&move_word(word).0),
            RedirOp::HereString => self.expand_string(word).map(Some),
            _ => self.redirect_word(word),
        }
    }

    /// `redirects` with their words and here-documents expanded, to be
    /// taken as they are: POSIX's order, in which a command's
    /// redirections are expanded before its assignments.
    pub fn expand_redirects(&mut self, redirects: &[Redirect]) -> Result<Vec<Redirect>, Flow> {
        let literal = |text: String| Word { parts: vec![WordPart::Quoted(text)] };
        let mut expanded = Vec::with_capacity(redirects.len());
        for r in redirects {
            let target = match &r.target {
                RedirTarget::Word(word) => {
                    RedirTarget::Expanded(word.clone(), self.target_text(r.op, word)?)
                }
                RedirTarget::HereDoc(doc) if doc.borrow().expand => {
                    let body = self.expand_quoted(&doc.borrow().body)?;
                    RedirTarget::HereDoc(Rc::new(RefCell::new(HereDoc {
                        body: literal(body),
                        expand: false,
                        ..doc.borrow().clone()
                    })))
                }
                RedirTarget::HereDoc(_) | RedirTarget::Expanded(..) => r.target.clone(),
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
        for entry in &mut self.held {
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
                    self.error_unlined(&format!(
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
        scope: Scope,
        saved: &mut Saved,
    ) -> Result<(), RedirError> {
        let save = scope == Scope::Command;
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
        let source = match r.op {
            RedirOp::HereDoc => {
                let RedirTarget::HereDoc(doc) = &r.target else { return Err(None) };
                let doc = doc.borrow().clone();
                let body = if doc.expand {
                    self.expand_quoted(&doc.body).map_err(Some)?
                } else {
                    doc.body
                        .as_literal()
                        .map_or_else(|| crate::show::word_text(&doc.body), str::to_string)
                };
                let body = match doc.body.parts.as_slice() {
                    [crate::ast::WordPart::Quoted(text)] if !doc.expand => text.clone(),
                    _ => body,
                };
                Some(self.here_document(&text::encode(&body))?)
            }
            RedirOp::HereString => {
                let mut body = match &r.target {
                    RedirTarget::Word(word) => self.expand_string(word).map_err(Some)?,
                    RedirTarget::Expanded(_, text) => text.clone().unwrap_or_default(),
                    RedirTarget::HereDoc(_) => return Err(None),
                };
                body.push('\n');
                Some(self.here_document(&text::encode(&body))?)
            }
            _ => None,
        };
        if let Some(source) = source {
            return self.install(source, fd, named, save, saved);
        }
        let (written, done) = match &r.target {
            RedirTarget::Word(word) => (word, None),
            RedirTarget::Expanded(word, text) => (word, Some(text.clone())),
            RedirTarget::HereDoc(_) => return Err(None),
        };
        if matches!(r.op, RedirOp::DupRead | RedirOp::DupWrite) {
            return self.duplicate(r, (fd, named), written, done, scope, saved);
        }
        let target = match done {
            Some(text) => text,
            None => self.target_text(r.op, written).map_err(Some)?,
        };
        let Some(target) = target else {
            self.error(&format!("{}: ambiguous redirect", show::word_text(written)));
            return Err(None);
        };
        if self.opts.restricted && r.op != RedirOp::Read {
            self.error(&format!("{target}: restricted: cannot redirect output"));
            return Err(None);
        }
        let opened = self.open_target(&target, r.op)?;
        if matches!(r.op, RedirOp::WriteBoth | RedirOp::AppendBoth) {
            return self.both(opened, save, saved);
        }
        self.install(opened, fd, named, save, saved)
    }

    /// `[n]>&word` and `[n]<&word`, as bash takes them: a close (`-`), a
    /// copy of a descriptor, a move (`n-`, which closes the one copied),
    /// or for `>&file`, `&>file`. A descriptor that is not open is named
    /// in the message as bash names it: by its number where it was written
    /// as one; for a move, by the descriptor redirected; otherwise by the
    /// word as written if the descriptor redirected is the usual one, and
    /// by that descriptor if not.
    fn duplicate(
        &mut self,
        r: &Redirect,
        (fd, named): (i32, Option<String>),
        written: &Word,
        done: Option<Option<String>>,
        scope: Scope,
        saved: &mut Saved,
    ) -> Result<(), RedirError> {
        let save = scope == Scope::Command;
        let default_fd = if r.op == RedirOp::DupRead { 0 } else { 1 };
        let (word, moved) = move_word(written);
        let literal = literal_fd(&word);
        let text = match done {
            Some(text) => text,
            None => self.target_text(r.op, written).map_err(Some)?,
        };
        // A move whose word comes to no word, or to more than one, closes
        // the descriptor in bash, as `-` does.
        let text = match text {
            Some(text) if text != "-" || moved => text,
            Some(_) => return self.close_redirected(fd, named.as_deref(), save, saved),
            None if moved => return self.close_redirected(fd, named.as_deref(), save, saved),
            None => {
                self.error(&format!("{}: ambiguous redirect", show::word_text(written)));
                return Err(None);
            }
        };
        let number = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        if !number && !text.is_empty() {
            if !moved && r.op == RedirOp::DupWrite && fd == default_fd && named.is_none() {
                // `>&file` is `&>file`.
                if self.opts.restricted {
                    self.error(&format!("{text}: restricted: cannot redirect output"));
                    return Err(None);
                }
                let opened = self.open_target(&text, RedirOp::Write)?;
                return self.both(opened, save, saved);
            }
            self.error(&format!("{text}: ambiguous redirect"));
            return Err(None);
        }
        let from = text
            .parse::<i32>()
            .ok()
            .filter(|&from| sys::is_open(from) && !self.held.contains(&Some(from)));
        let Some(from) = from else {
            let name = match literal {
                Some(n) => n.to_string(),
                None if named.is_some() || !moved && fd == default_fd => show::word_text(written),
                None => fd.to_string(),
            };
            // bash first fails to copy the descriptor for a variable, or
            // one a move in the shell would put back.
            if named.is_some() || moved && scope != Scope::Child && sys::is_open(fd) {
                self.error_unlined("redirection error: cannot duplicate fd: Bad file descriptor");
            }
            self.error(&format!("{name}: Bad file descriptor"));
            return Err(None);
        };
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
        if moved {
            self.save_fd(from, save, saved)?;
            sys::close(from);
            if !save {
                self.coproc_fd_changed(from, true);
            }
        }
        Ok(())
    }

    /// `>&-`: closes `fd`, or the descriptor in `{name}`'s variable.
    fn close_redirected(
        &mut self,
        fd: i32,
        named: Option<&str>,
        save: bool,
        saved: &mut Saved,
    ) -> Result<(), RedirError> {
        let fd = match named {
            Some(name) => {
                let name = self.expand_element(name).map_err(Some)?;
                match self.get(&name) {
                    Some(value) => match value.parse() {
                        Ok(n) => n,
                        // bash leaves a variable that holds no number.
                        Err(_) => return Ok(()),
                    },
                    None => {
                        self.error(&format!("{name}: ambiguous redirect"));
                        return Err(None);
                    }
                }
            }
            None => fd,
        };
        self.save_fd(fd, save, saved)?;
        sys::close(fd);
        if !save {
            self.coproc_fd_changed(fd, true);
        }
        Ok(())
    }

    /// `&>file`: `opened` on standard output and standard error.
    fn both(&mut self, opened: i32, save: bool, saved: &mut Saved) -> Result<(), RedirError> {
        let copy = sys::dup_high(opened).ok();
        self.install(opened, 1, None, save, saved)?;
        if let Some(copy) = copy {
            self.install(copy, 2, None, save, saved)?;
        }
        Ok(())
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
                return self.open_noclobber(path, mode);
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

    /// Opens `path` for `>` under `set -C`, which leaves a regular file
    /// alone; /dev/null and the like are written to. A file that is not
    /// there is made with O_EXCL, so one made in between fails the
    /// redirection, as `set -C; echo $$ > lock` relies on to take a lock.
    /// One that is there is opened without O_TRUNC, and must still be the
    /// same file, not a regular one, once open.
    fn open_noclobber(&mut self, path: &str, mode: u32) -> Result<i32, RedirError> {
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(text::to_os(path)).ok();
        let opened = match &before {
            Some(meta) if meta.is_file() => Err(None),
            Some(_) => sys::open(path, libc::O_WRONLY | libc::O_CREAT, mode).map_err(Some),
            None => {
                sys::open(path, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, mode).map_err(Some)
            }
        };
        match opened {
            Ok(fd) => {
                let Some(meta) = before else { return Ok(fd) };
                if sys::identity(fd) == Some((meta.dev(), meta.ino(), false)) {
                    return Ok(fd);
                }
                sys::close(fd);
            }
            Err(Some(error)) if error.raw_os_error() != Some(libc::EEXIST) => {
                self.error(&format!("{path}: {}", sys::describe(&error)));
                return Err(None);
            }
            Err(_) => {}
        }
        self.error(&format!("{path}: cannot overwrite existing file"));
        Err(None)
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
            let element = match self.expand_element(&name) {
                Ok(element) => element,
                Err(flow) => {
                    sys::close(new);
                    return Err(Some(flow));
                }
            };
            let assigned = match element.split_once('[') {
                Some((_, "@]" | "*]")) => Err(format!("{name}: bad array subscript")),
                _ => self.assign(&element, new.to_string(), false),
            };
            if let Err(message) = assigned {
                sys::close(new);
                self.error(&message);
                self.error(&format!("{name}: cannot assign fd to variable"));
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
        if !save {
            self.coproc_fd_changed(fd, false);
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
