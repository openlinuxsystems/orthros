// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `coproc`: a command in the background whose standard input and output
//! are pipes to the shell, run as bash runs it.
//!
//! The shell's ends, the one that reads the command's output and the one
//! that writes its input, go in the indexed array `NAME`, and the process
//! in `NAME_PID` and `$!`. They sit at the highest free descriptors under
//! 64, closed on exec; the children that run shell code in a subshell
//! (`( )`, a pipeline's commands, `&`, another coprocess) close them, as
//! bash's do, while command and process substitutions keep them. Once the
//! shell has seen the command end, when it waits for a job or starts
//! another, the descriptors close and the variables go.

use std::collections::BTreeMap;

use crate::ast::{Command, Coproc};
use crate::jobs::Proc;
use crate::parse::is_name;
use crate::shell::{Res, Shell};
use crate::vars::{Value, attr};
use crate::{show, sys, trap};

/// A coprocess the shell started.
#[derive(Clone, Debug)]
pub struct Active {
    pub name: String,
    pub pid: i32,
    /// The shell's ends, `NAME[0]` and `NAME[1]`: -1 once closed.
    pub fds: [i32; 2],
    /// The shell has seen it end.
    pub ended: bool,
    /// Started by the shell this one is a child of: it is not this
    /// shell's to wait for or clean up.
    pub inherited: bool,
}

/// Moves `fd` to the highest free descriptor under 64, as bash places a
/// coprocess's pipes; where none is free above 3, it stays.
fn move_high(fd: i32) -> i32 {
    match (4..64).rev().find(|&high| !sys::is_open(high)) {
        Some(high) if sys::dup2(fd, high).is_ok() => {
            sys::close(fd);
            high
        }
        _ => fd,
    }
}

/// A pipe, its read end then its write end, moved high.
fn high_pipe() -> std::io::Result<(i32, i32)> {
    let (read, write) = sys::pipe()?;
    let read = move_high(read);
    Ok((read, move_high(write)))
}

impl Shell {
    pub fn run_coproc(&mut self, coproc: &Coproc) -> Res {
        self.lineno = coproc.line;
        let name = &coproc.name;
        if !is_name(name) {
            self.error(&format!("`{name}': not a valid identifier"));
            return Ok(1);
        }
        if let Some(last) = self.jobs.coprocs.last()
            && last.fds.iter().any(|&fd| fd >= 0)
        {
            self.error(&format!(
                "warning: execute_coproc: coproc [{}:{}] still exists",
                last.pid, last.name
            ));
        }
        let pipes = high_pipe().and_then(|output| match high_pipe() {
            Ok(input) => Ok((output, input)),
            Err(error) => {
                sys::close(output.0);
                sys::close(output.1);
                Err(error)
            }
        });
        // The command's output, read by the shell, and its input.
        let ((from, to_shell), (to_command, into)) = match pipes {
            Ok(pipes) => pipes,
            Err(error) => {
                self.error(&format!("pipe error: {}", sys::describe(&error)));
                return Ok(1);
            }
        };
        let monitor = self.opts.monitor;
        let pid = self.fork_child(0, true)?;
        if pid == 0 {
            sys::close(from);
            sys::close(into);
            self.close_coprocs();
            self.forget_jobs();
            if !monitor {
                // Without job control, Ctrl-C is not for background jobs.
                trap::set(libc::SIGINT, trap::Disposition::Ignore);
                trap::set(libc::SIGQUIT, trap::Disposition::Ignore);
            }
            let _ = sys::dup2(to_command, 0);
            let _ = sys::dup2(to_shell, 1);
            for fd in [to_command, to_shell] {
                if fd > 1 {
                    sys::close(fd);
                }
            }
            let result = self.run_command(&coproc.command, true);
            self.exit_child(result);
        }
        sys::close(to_shell);
        sys::close(to_command);
        let fds = [from, into];
        for fd in fds {
            let _ = sys::set_cloexec(fd, true);
        }
        self.last_bg = Some(pid);
        self.reap();
        let text = show::job_line(&Command::Coproc(Box::new(coproc.clone())));
        let id = self.add_job(pid, vec![Proc::new(pid, text.clone())], text, true);
        if self.interactive {
            let _ = sys::write_all(2, format!("[{id}] {pid}\n").as_bytes());
        }
        self.jobs.coprocs.push(Active {
            name: name.clone(),
            pid,
            fds,
            // It may have ended already.
            ended: self.jobs.get(id).is_some_and(|j| j.done()),
            inherited: false,
        });
        let array = self.resolve(name);
        match self.writable(&array) {
            Ok(()) => {
                // Whatever it was, it is an indexed array now, as in bash.
                let var = self.vars.entry(&array);
                var.attrs &= !attr::ASSOC;
                var.value = Some(Value::Array(BTreeMap::from([
                    (0, from.to_string()),
                    (1, into.to_string()),
                ])));
            }
            Err(message) => self.error(&message),
        }
        if let Err(message) = self.assign(&format!("{name}_PID"), pid.to_string(), false) {
            self.error(&message);
        }
        Ok(0)
    }

    /// In a child that runs shell code: closes the coprocesses' ends,
    /// which are its parent's.
    pub fn close_coprocs(&mut self) {
        for coproc in self.jobs.coprocs.drain(..) {
            for fd in coproc.fds {
                if fd >= 0 {
                    sys::close(fd);
                }
            }
        }
    }

    /// Closes the ends of the coprocesses seen to end, and unsets their
    /// variables.
    pub fn reap_coprocs(&mut self) {
        if !self.jobs.coprocs.iter().any(|c| c.ended && !c.inherited) {
            return;
        }
        let (ended, running): (Vec<Active>, Vec<Active>) = std::mem::take(&mut self.jobs.coprocs)
            .into_iter()
            .partition(|c| c.ended && !c.inherited);
        self.jobs.coprocs = running;
        for coproc in ended {
            for fd in coproc.fds {
                if fd >= 0 {
                    sys::close(fd);
                }
            }
            for name in [coproc.name.clone(), format!("{}_PID", coproc.name)] {
                let name = self.resolve(&name);
                if !self.vars.unset(&name) {
                    self.error(&format!("{name}: cannot unset: readonly variable"));
                }
            }
        }
    }

    /// Notes that a redirection the shell keeps closed descriptor `fd`:
    /// a coprocess's end there is gone, and its element says -1, as in
    /// bash. One that only put something else there stops being the
    /// coprocess's, though its element stays, as bash leaves it.
    pub fn coproc_fd_changed(&mut self, fd: i32, closed: bool) {
        let mut marked = Vec::new();
        for coproc in &mut self.jobs.coprocs {
            for (i, end) in coproc.fds.iter_mut().enumerate() {
                if *end == fd {
                    *end = -1;
                    if closed {
                        marked.push((coproc.name.clone(), i as i64));
                    }
                }
            }
        }
        for (name, i) in marked {
            let array = self.resolve(&name);
            if let Some(var) = self.vars.get_mut(&array)
                && let Some(Value::Array(map)) = &mut var.value
            {
                map.insert(i, "-1".to_string());
            }
        }
    }
}
