// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Jobs: the processes of each pipeline the shell started, and waiting
//! for them.
//!
//! With job control (`set -m`, on in interactive shells) each job is a
//! process group of its own; the one in the foreground owns the terminal,
//! and when it stops (Ctrl-Z) or ends, the shell takes the terminal back.
//! Without job control, the processes stay in the shell's group.

use std::collections::HashMap;

use crate::shell::{Flow, Shell};
use crate::{sys, trap};

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    /// The wait status, once it ended.
    pub status: Option<i32>,
    pub stopped: bool,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: usize,
    pub pgid: i32,
    pub procs: Vec<Proc>,
    /// The command, as `jobs` shows it.
    pub text: String,
    /// The change in state not yet reported.
    pub notify: bool,
    /// `disown -h`: not sent SIGHUP when the shell hangs up.
    pub nohup: bool,
    pub background: bool,
}

impl Job {
    pub fn done(&self) -> bool {
        self.procs.iter().all(|p| p.status.is_some())
    }

    pub fn stopped(&self) -> bool {
        !self.done() && self.procs.iter().filter(|p| p.status.is_none()).all(|p| p.stopped)
    }

    /// The job's status: its last process's, or with `pipefail`, the last
    /// that failed.
    pub fn status(&self, pipefail: bool) -> i32 {
        let statuses: Vec<i32> =
            self.procs.iter().map(|p| status_of(p.status.unwrap_or(0))).collect();
        if pipefail && let Some(failed) = statuses.iter().rev().find(|s| **s != 0) {
            return *failed;
        }
        statuses.last().copied().unwrap_or(0)
    }

    /// `Running`, `Done`, `Stopped` and the like, as `jobs` prints it.
    pub fn state(&self) -> String {
        if self.stopped() {
            let signal =
                self.procs.iter().find_map(|p| p.stopped.then_some(()).and(Some(libc::SIGTSTP)));
            return match signal.and_then(trap::name) {
                Some("TSTP") | None => "Stopped".into(),
                Some(name) => format!("Stopped (SIG{name})"),
            };
        }
        if !self.done() {
            return "Running".into();
        }
        let last = self.procs.last().and_then(|p| p.status).unwrap_or(0);
        describe_status(last)
    }
}

/// The text for how a process ended: `Done`, `Exit 2`, `Terminated`.
pub fn describe_status(status: i32) -> String {
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        let mut text = signal_description(signal);
        if libc::WCOREDUMP(status) {
            text.push_str(" (core dumped)");
        }
        return text;
    }
    match libc::WEXITSTATUS(status) {
        0 => "Done".into(),
        code => format!("Exit {code}"),
    }
}

/// What a signal means, as the shell reports a process it ended.
pub fn signal_description(signal: i32) -> String {
    match signal {
        libc::SIGHUP => "Hangup".into(),
        libc::SIGINT => "Interrupt".into(),
        libc::SIGQUIT => "Quit".into(),
        libc::SIGILL => "Illegal instruction".into(),
        libc::SIGTRAP => "Trace/breakpoint trap".into(),
        libc::SIGABRT => "Aborted".into(),
        libc::SIGBUS => "Bus error".into(),
        libc::SIGFPE => "Floating point exception".into(),
        libc::SIGKILL => "Killed".into(),
        libc::SIGUSR1 => "User defined signal 1".into(),
        libc::SIGSEGV => "Segmentation fault".into(),
        libc::SIGUSR2 => "User defined signal 2".into(),
        libc::SIGPIPE => "Broken pipe".into(),
        libc::SIGALRM => "Alarm clock".into(),
        libc::SIGTERM => "Terminated".into(),
        libc::SIGXCPU => "CPU time limit exceeded".into(),
        libc::SIGXFSZ => "File size limit exceeded".into(),
        _ => match trap::name(signal) {
            Some(name) => format!("SIG{name}"),
            None => format!("Signal {signal}"),
        },
    }
}

/// A wait status as the shell's `$?`: the exit code, or 128 plus the
/// signal that ended or stopped it.
pub fn status_of(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else if libc::WIFSTOPPED(status) {
        128 + libc::WSTOPSIG(status)
    } else {
        0
    }
}

#[derive(Debug, Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    /// `%+` and `%-`.
    pub current: Option<usize>,
    pub previous: Option<usize>,
    /// How background processes no job holds ended, for `wait pid`.
    pub reaped: HashMap<i32, i32>,
}

impl Jobs {
    pub fn get(&self, id: usize) -> Option<&Job> {
        self.list.iter().find(|j| j.id == id)
    }

    pub fn get_mut(&mut self, id: usize) -> Option<&mut Job> {
        self.list.iter_mut().find(|j| j.id == id)
    }

    fn next_id(&self) -> usize {
        self.list.iter().map(|j| j.id).max().unwrap_or(0) + 1
    }

    /// Makes `id` the current job.
    pub fn make_current(&mut self, id: usize) {
        if self.current != Some(id) {
            self.previous = self.current;
            self.current = Some(id);
        }
    }

    pub fn remove(&mut self, id: usize) {
        self.list.retain(|j| j.id != id);
        if self.current == Some(id) {
            self.current = self.previous.take();
        }
        if self.previous == Some(id) {
            self.previous = None;
        }
        if self.current.is_none() {
            // The most recent job left becomes current.
            self.current = self.list.iter().rev().map(|j| j.id).next();
        }
        if self.previous.is_none() || self.previous == self.current {
            self.previous =
                self.list.iter().rev().map(|j| j.id).find(|id| Some(*id) != self.current);
        }
    }

    /// Records a process's new wait status; false if no job holds it.
    pub fn record(&mut self, pid: i32, status: i32) -> bool {
        for job in &mut self.list {
            if let Some(proc) = job.procs.iter_mut().find(|p| p.pid == pid) {
                if libc::WIFSTOPPED(status) {
                    proc.stopped = true;
                    job.notify = true;
                } else if libc::WIFCONTINUED(status) {
                    proc.stopped = false;
                } else {
                    proc.status = Some(status);
                    proc.stopped = false;
                    if job.done() {
                        job.notify = true;
                    }
                }
                return true;
            }
        }
        false
    }

    /// The job a `%` specification names: `%1`, `%%`, `%+`, `%-`,
    /// `%name` (the command starts with it), `%?text` (contains it).
    pub fn find(&self, spec: &str) -> Result<usize, String> {
        let body = spec.strip_prefix('%').unwrap_or(spec);
        let found = match body {
            "" | "%" | "+" => self.current,
            "-" => self.previous,
            _ if body.chars().all(|c| c.is_ascii_digit()) => {
                let id: usize = body.parse().unwrap_or(0);
                self.get(id).map(|j| j.id)
            }
            _ => {
                let matching: Vec<usize> = match body.strip_prefix('?') {
                    Some(text) => {
                        self.list.iter().filter(|j| j.text.contains(text)).map(|j| j.id).collect()
                    }
                    None => self
                        .list
                        .iter()
                        .filter(|j| j.text.starts_with(body))
                        .map(|j| j.id)
                        .collect(),
                };
                if matching.len() > 1 {
                    return Err(format!("{spec}: ambiguous job spec"));
                }
                matching.first().copied()
            }
        };
        found.filter(|id| self.get(*id).is_some()).ok_or_else(|| format!("{spec}: no such job"))
    }
}

impl Shell {
    /// Adds a job of the processes `pids`, in group `pgid`.
    pub fn add_job(&mut self, pgid: i32, pids: &[i32], text: String, background: bool) -> usize {
        let id = self.jobs.next_id();
        let procs = pids.iter().map(|&pid| Proc { pid, status: None, stopped: false }).collect();
        self.jobs.list.push(Job { id, pgid, procs, text, notify: false, nohup: false, background });
        if background {
            self.jobs.make_current(id);
        }
        id
    }

    /// Waits for a foreground job to end or stop; its status.
    pub fn wait_job(&mut self, id: usize) -> Result<i32, Flow> {
        let monitor = self.opts.monitor && self.tty.is_some();
        if monitor && let Some(job) = self.jobs.get(id) {
            let pgid = job.pgid;
            self.give_terminal(pgid);
        }
        while let Some(job) = self.jobs.get(id) {
            if job.done() || job.stopped() {
                break;
            }
            let flags = if monitor { libc::WUNTRACED } else { 0 };
            // Any child: one of this job's, or a background one that ends
            // meanwhile.
            match sys::waitpid(-1, flags) {
                Ok(Some((pid, status))) => {
                    if !self.jobs.record(pid, status) {
                        self.jobs.reaped.insert(pid, status);
                    }
                }
                Ok(None) => {}
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => {}
                Err(_) => {
                    // No children left: whatever is not marked ended did.
                    if let Some(job) = self.jobs.get_mut(id) {
                        for proc in &mut job.procs {
                            proc.status.get_or_insert(0);
                        }
                    }
                    break;
                }
            }
        }
        if monitor {
            self.take_terminal();
        }
        let Some(job) = self.jobs.get(id).cloned() else { return Ok(0) };
        self.pipestatus = job.procs.iter().map(|p| status_of(p.status.unwrap_or(0))).collect();
        if job.stopped() {
            if let Some(job) = self.jobs.get_mut(id) {
                job.background = true;
                job.notify = false;
            }
            self.jobs.make_current(id);
            let job = self.jobs.get(id).cloned();
            if let Some(job) = job {
                self.print_job(&job, false);
            }
            return Ok(128 + libc::SIGTSTP);
        }
        let status = job.status(self.opts.pipefail);
        // A job a signal ended is reported, as bash does; SIGINT and
        // SIGPIPE are not news.
        if let Some(last) = job.procs.last().and_then(|p| p.status)
            && libc::WIFSIGNALED(last)
            && !matches!(libc::WTERMSIG(last), libc::SIGINT | libc::SIGPIPE)
        {
            let mut text = describe_status(last);
            if self.interactive {
                text.push('\n');
            } else {
                text = format!(
                    "{}: line {}: {} {}\n",
                    self.source,
                    self.lineno,
                    job.procs.last().map_or(0, |p| p.pid),
                    text
                );
            }
            let _ = sys::write_all(2, crate::text::encode(&text).as_slice());
        }
        if self.interactive
            && job
                .procs
                .last()
                .and_then(|p| p.status)
                .is_some_and(|s| libc::WIFSIGNALED(s) && libc::WTERMSIG(s) == libc::SIGINT)
        {
            let _ = sys::write_all(2, b"\n");
        }
        self.jobs.remove(id);
        Ok(status)
    }

    /// Waits for the process `pid` alone, outside any job: a command
    /// substitution or a subshell.
    pub fn wait_pid(&mut self, pid: i32) -> i32 {
        loop {
            match sys::waitpid(pid, 0) {
                Ok(Some((_, status))) => return status_of(status),
                Ok(None) => {}
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => {}
                Err(_) => return self.jobs.reaped.remove(&pid).map_or(127, status_of),
            }
        }
    }

    /// Notes every child that changed state, without waiting.
    pub fn reap(&mut self) {
        if self.jobs.list.is_empty() {
            return;
        }
        let flags = libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED;
        while let Ok(Some((pid, status))) = sys::waitpid(-1, flags) {
            if !self.jobs.record(pid, status) {
                self.jobs.reaped.insert(pid, status);
            }
        }
    }

    /// Reports the background jobs that ended or stopped, as an
    /// interactive shell does before its prompt, and forgets those that
    /// ended.
    pub fn notify_jobs(&mut self) {
        self.reap();
        let changed: Vec<Job> = self.jobs.list.iter().filter(|j| j.notify).cloned().collect();
        for job in changed {
            if self.interactive {
                self.print_job(&job, false);
            }
            if let Some(j) = self.jobs.get_mut(job.id) {
                j.notify = false;
            }
            if job.done() {
                if !self.interactive {
                    // Keep the statuses for a later `wait pid`.
                    for proc in &job.procs {
                        if let Some(status) = proc.status {
                            self.jobs.reaped.insert(proc.pid, status);
                        }
                    }
                }
                self.jobs.remove(job.id);
            }
        }
    }

    /// Forgets job `id`, which `wait` saw end. An interactive shell says
    /// how it ended first, as it would have before the next prompt.
    pub fn forget_waited(&mut self, id: usize) {
        if self.interactive
            && let Some(job) = self.jobs.get(id).cloned()
            && job.notify
        {
            self.print_job(&job, false);
        }
        self.jobs.remove(id);
    }

    /// Prints a job as `jobs` does: `[1]+  Running   sleep 10 &`.
    pub fn print_job(&self, job: &Job, pids: bool) {
        let mark = if self.jobs.current == Some(job.id) {
            '+'
        } else if self.jobs.previous == Some(job.id) {
            '-'
        } else {
            ' '
        };
        let state = job.state();
        let suffix = if state == "Running" { " &" } else { "" };
        let line = if pids {
            format!(
                "[{}]{mark}  {} {state:<24}{}{suffix}\n",
                job.id,
                job.procs.first().map_or(0, |p| p.pid),
                job.text
            )
        } else {
            format!("[{}]{mark}  {state:<24}{}{suffix}\n", job.id, job.text)
        };
        let _ = sys::write_all(if pids { 1 } else { 2 }, crate::text::encode(&line).as_slice());
    }

    /// Gives the terminal to process group `pgid`.
    pub fn give_terminal(&mut self, pgid: i32) {
        if let Some(tty) = self.tty {
            let _ = sys::tcsetpgrp(tty, pgid);
        }
    }

    /// Takes the terminal back for the shell, and its modes as they were.
    pub fn take_terminal(&mut self) {
        if let Some(tty) = self.tty {
            let _ = sys::tcsetpgrp(tty, self.shell_pgid);
            if let Some(modes) = &self.tty_modes {
                sys::tcsetattr(tty, modes);
            }
        }
    }

    /// Continues job `id`, in the foreground or the background.
    pub fn continue_job(&mut self, id: usize, foreground: bool) -> Result<i32, Flow> {
        let Some(job) = self.jobs.get_mut(id) else { return Ok(1) };
        job.background = !foreground;
        for proc in &mut job.procs {
            proc.stopped = false;
        }
        let pgid = job.pgid;
        let text = job.text.clone();
        self.jobs.make_current(id);
        if foreground {
            let _ = sys::write_all(1, crate::text::encode(&format!("{text}\n")).as_slice());
            if self.opts.monitor && self.tty.is_some() {
                self.give_terminal(pgid);
            }
        } else {
            let _ =
                sys::write_all(1, crate::text::encode(&format!("[{id}]+ {text} &\n")).as_slice());
        }
        if let Some(job) = self.jobs.get(id) {
            self.signal_job(job, libc::SIGCONT);
        }
        if foreground {
            return self.wait_job(id);
        }
        Ok(0)
    }

    /// The process numbers that reach a job: its process group's, as a
    /// negative number, when jobs have groups of their own (job control on
    /// a terminal), else each of its processes still running. `set -m`
    /// in a script without a terminal tracks jobs, but makes no groups.
    pub fn job_targets(&self, job: &Job) -> Vec<i32> {
        if self.opts.monitor && self.tty.is_some() {
            vec![-job.pgid]
        } else {
            job.procs.iter().filter(|p| p.status.is_none()).map(|p| p.pid).collect()
        }
    }

    /// Sends `signal` to a job's processes.
    pub fn signal_job(&self, job: &Job, signal: i32) {
        for target in self.job_targets(job) {
            let _ = sys::kill(target, signal);
        }
    }

    /// Sends SIGHUP to every job (and SIGCONT to the stopped ones), as a
    /// shell that hangs up does.
    pub fn hang_up_jobs(&mut self) {
        for job in &self.jobs.list {
            if job.nohup {
                continue;
            }
            self.signal_job(job, libc::SIGHUP);
            if job.stopped() {
                self.signal_job(job, libc::SIGCONT);
            }
        }
    }
}
