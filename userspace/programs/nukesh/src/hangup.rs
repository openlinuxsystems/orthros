// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A hang-up at the prompt (ADR 0267). When the terminal goes away while
//! the line editor waits, its terminal library reads nothing forever and
//! never returns, so the shell would spin rather than reach the place
//! where it handles SIGHUP. So before each prompt the shell readies what
//! a hang-up must do, and SIGHUP at the prompt does it from the signal
//! handler, with only async-signal-safe calls: SIGHUP (and SIGCONT to the
//! stopped) to each job not marked by `disown -h`, the history entries
//! not yet saved appended to `HISTFILE`, and the exit, with status 129.
//! The EXIT trap does not run then; elsewhere SIGHUP is handled as
//! before.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::shell::Shell;

/// What a hang-up at the prompt does, made ready before it.
struct Ready {
    /// Each job's targets, and whether it is stopped.
    jobs: Vec<(i32, bool)>,
    history: Option<(CString, Vec<u8>)>,
}

static AT_PROMPT: AtomicBool = AtomicBool::new(false);
static READY: AtomicPtr<Ready> = AtomicPtr::new(std::ptr::null_mut());

/// Before the prompt: readies the hang-up, then marks the shell as
/// waiting at it.
pub fn enter_prompt(shell: &Shell) {
    let mut jobs = Vec::new();
    for job in shell.jobs.list.iter().filter(|j| !j.nohup && !j.done()) {
        for target in job.targets() {
            jobs.push((target, job.stopped()));
        }
    }
    let history = shell
        .interactive
        .then(|| shell.history_file())
        .flatten()
        .and_then(|path| {
            let new = shell.history.unsaved();
            if new.is_empty() {
                return None;
            }
            let data = crate::text::encode(&crate::history::format_entries(new));
            Some((CString::new(path).ok()?, data))
        });
    let ready = Box::into_raw(Box::new(Ready { jobs, history }));
    let old = READY.swap(ready, Ordering::SeqCst);
    if !old.is_null() {
        // SAFETY: made by Box::into_raw above, and the handler uses it only
        // while AT_PROMPT is set, which it is not now.
        drop(unsafe { Box::from_raw(old) });
    }
    AT_PROMPT.store(true, Ordering::SeqCst);
}

/// After the prompt.
pub fn leave_prompt() {
    AT_PROMPT.store(false, Ordering::SeqCst);
}

/// From the SIGHUP handler: hangs up now if the shell waits at its
/// prompt; returns otherwise.
pub fn at_prompt_hang_up() {
    if !AT_PROMPT.load(Ordering::SeqCst) {
        return;
    }
    let ready = READY.load(Ordering::SeqCst);
    if !ready.is_null() {
        // SAFETY: set by enter_prompt and not freed while AT_PROMPT is set;
        // only read here.
        let ready = unsafe { &*ready };
        for (target, stopped) in &ready.jobs {
            // SAFETY: kill, open, write and close are async-signal-safe.
            unsafe {
                libc::kill(*target, libc::SIGHUP);
                if *stopped {
                    libc::kill(*target, libc::SIGCONT);
                }
            }
        }
        if let Some((path, data)) = &ready.history {
            // SAFETY: as above; the path is a C string of ours.
            unsafe {
                let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT, 0o600);
                if fd >= 0 {
                    libc::write(fd, data.as_ptr().cast(), data.len());
                    libc::close(fd);
                }
            }
        }
    }
    // SAFETY: _exit is async-signal-safe.
    unsafe { libc::_exit(128 + libc::SIGHUP) };
}
