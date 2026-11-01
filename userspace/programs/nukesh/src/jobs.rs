// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Jobs: the processes of each pipeline the shell started, and waiting
//! for them.
//!
//! With job control (`set -m`, on in interactive shells) each job is a
//! process group of its own; the one in the foreground owns the terminal,
//! and when it stops (Ctrl-Z) or ends, the shell takes the terminal back.
//! Without job control, the processes stay in the shell's group.

use indexmap::IndexMap;

use crate::shell::{Flow, Shell};
use crate::{sys, trap};

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    /// The wait status, once it ended.
    pub status: Option<i32>,
    pub stopped: bool,
    /// Its command, as a report of its end shows it.
    pub text: String,
}

impl Proc {
    pub fn new(pid: i32, text: String) -> Proc {
        Proc { pid, status: None, stopped: false, text }
    }
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
    /// Whether the job has process group `pgid` of its own: it started
    /// under job control.
    pub grouped: bool,
    /// The parent shell's, listed in a command substitution or a
    /// pipeline's command as bash lists them, but not this shell's to
    /// wait for.
    pub inherited: bool,
    /// A script's background job that ended, kept until `jobs` lists it
    /// or `wait` takes it.
    pub held: bool,
    /// `jobs` has listed its end: it goes before the next command or
    /// job, as in bash, and is not listed again.
    pub told: bool,
}

impl Job {
    /// The process numbers that reach the job: its process group's, as a
    /// negative number, when it has a group of its own (it started under
    /// job control), else each of its processes still running.
    pub fn targets(&self) -> Vec<i32> {
        if self.grouped {
            vec![-self.pgid]
        } else {
            self.procs.iter().filter(|p| p.status.is_none()).map(|p| p.pid).collect()
        }
    }

    /// Sends `signal` to the job's processes.
    pub fn signal(&self, signal: i32) {
        for target in self.targets() {
            let _ = sys::kill(target, signal);
        }
    }

    pub fn done(&self) -> bool {
        self.procs.iter().all(|p| p.status.is_some())
    }

    pub fn stopped(&self) -> bool {
        !self.done() && self.procs.iter().filter(|p| p.status.is_none()).all(|p| p.stopped)
    }

    pub fn running(&self) -> bool {
        !self.done() && !self.stopped()
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
            return match signal.and_then(trap::name).as_deref() {
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

    /// The processes of a job a signal ended, as a non-interactive bash
    /// lists them: the PID, how it ended and the command of each, those
    /// after the first after `|`; an end the same as the first's is left
    /// blank.
    pub fn listing(&self) -> String {
        let mut out = String::new();
        let first = self.procs.first().and_then(|p| p.status);
        for (i, proc) in self.procs.iter().enumerate() {
            let status = proc.status.unwrap_or(0);
            if i > 0 {
                out.push_str("     ");
            }
            out.push_str(&format!("{:>5} ", proc.pid));
            if i > 0 && proc.status == first {
                out.push_str(&" ".repeat(22));
            } else {
                let state = if libc::WIFSIGNALED(status) {
                    signal_description(libc::WTERMSIG(status))
                } else {
                    describe_status(status)
                };
                out.push_str(&format!("{state:<24}"));
                if libc::WIFSIGNALED(status) && libc::WCOREDUMP(status) {
                    out.push_str("(core dumped) ");
                }
            }
            if i > 0 {
                out.push_str("| ");
            }
            out.push_str(&proc.text);
            out.push('\n');
        }
        out
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
    pub reaped: Reaped,
    /// The coprocesses, which outlive their jobs (`disown`).
    pub coprocs: Vec<crate::coproc::Active>,
}

/// How background processes no job holds ended, oldest first. As in
/// bash, only the last CHILD_MAX or so are kept (at least 4096, at most
/// 32768), so a script that starts processes it never waits for does not
/// grow for ever.
#[derive(Debug, Default)]
pub struct Reaped {
    statuses: IndexMap<i32, i32>,
}

impl Reaped {
    pub fn insert(&mut self, pid: i32, status: i32) {
        self.statuses.insert(pid, status);
        // A few over the limit go at once, so that keeping to it costs
        // little.
        let limit = child_max();
        if self.statuses.len() > limit + limit / 8 {
            let excess = self.statuses.len() - limit;
            self.statuses.drain(..excess);
        }
    }

    pub fn remove(&mut self, pid: i32) -> Option<i32> {
        self.statuses.swap_remove(&pid)
    }

    /// The one kept longest.
    pub fn oldest(&self) -> Option<(i32, i32)> {
        self.statuses.first().map(|(&pid, &status)| (pid, status))
    }

    pub fn clear(&mut self) {
        self.statuses.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.statuses.len()
    }
}

/// How many ended processes' statuses the shell keeps: CHILD_MAX, held
/// to bash's bounds.
fn child_max() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        // Dukovo sets no per-user process limit: mlibc's sysconf only
        // warns on the console and says 25, which the floor raises anyway.
        if cfg!(target_os = "radix") {
            return 4096;
        }
        // SAFETY: sysconf only reads a limit.
        let limit = unsafe { libc::sysconf(libc::_SC_CHILD_MAX) };
        usize::try_from(limit).unwrap_or(0).clamp(4096, 32768)
    })
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

    /// The newest job before `below` that passes `test`.
    fn newest(&self, test: fn(&Job) -> bool, below: usize) -> Option<usize> {
        self.list.iter().filter(|j| j.id < below && test(j)).map(|j| j.id).max()
    }

    /// Makes `id` the current job, and picks the previous one as bash
    /// does: the old current job if it is stopped; else, for a stopped
    /// current job, the newest stopped one before it; else the newest
    /// running job before a running current one, or at all; else the
    /// current job itself. A job that ended is never `%-`.
    pub fn make_current(&mut self, id: usize) {
        if self.current != Some(id) {
            self.previous = self.current;
            self.current = Some(id);
        }
        if self.previous != self.current
            && self.previous.and_then(|p| self.get(p)).is_some_and(Job::stopped)
        {
            return;
        }
        let current = self.get(id);
        let (stopped, running) =
            (current.is_some_and(Job::stopped), current.is_some_and(Job::running));
        let candidate = stopped
            .then(|| self.newest(Job::stopped, id))
            .flatten()
            .or_else(|| self.newest(Job::running, if running { id } else { usize::MAX }));
        self.previous = candidate.or(self.current);
    }

    pub fn remove(&mut self, id: usize) {
        self.list.retain(|j| j.id != id);
        if self.current == Some(id) || self.previous == Some(id) {
            self.reset_current();
        }
    }

    /// Picks the current job again after one goes, as bash does: a
    /// stopped current job stays; else the previous one if stopped, the
    /// newest stopped job, or the newest running one; else none.
    fn reset_current(&mut self) {
        let stopped = |id: Option<usize>| id.filter(|&id| self.get(id).is_some_and(Job::stopped));
        let candidate = stopped(self.current)
            .or_else(|| stopped(self.previous))
            .or_else(|| self.newest(Job::stopped, usize::MAX))
            .or_else(|| self.newest(Job::running, usize::MAX));
        match candidate {
            Some(id) => self.make_current(id),
            None => {
                self.current = None;
                self.previous = None;
            }
        }
    }

    /// Notes a child's new wait status: in its job, or for one no job
    /// holds, for a later `wait pid`.
    pub fn note(&mut self, pid: i32, status: i32) {
        if !libc::WIFSTOPPED(status) && !libc::WIFCONTINUED(status) {
            for coproc in self.coprocs.iter_mut().filter(|c| c.pid == pid) {
                coproc.ended = true;
            }
        }
        if !self.record(pid, status) {
            self.reaped.insert(pid, status);
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
    /// Adds a job of the processes `procs`, in group `pgid`.
    pub fn add_job(
        &mut self,
        pgid: i32,
        procs: Vec<Proc>,
        text: String,
        background: bool,
    ) -> usize {
        // Jobs `jobs` told of and coprocesses seen to end go as a new job
        // starts, as in bash.
        self.forget_told();
        self.reap_coprocs();
        let id = self.jobs.next_id();
        let grouped = self.opts.monitor;
        self.jobs.list.push(Job {
            id,
            pgid,
            procs,
            text,
            notify: false,
            nohup: false,
            background,
            grouped,
            inherited: false,
            held: false,
            told: false,
        });
        // A process may have ended, and been seen, before its job was made.
        for pid in
            self.jobs.get(id).map_or_default(|j| j.procs.iter().map(|p| p.pid).collect::<Vec<_>>())
        {
            if let Some(status) = self.jobs.reaped.remove(pid) {
                self.jobs.record(pid, status);
            }
        }
        if background {
            self.jobs.make_current(id);
        }
        id
    }

    /// In a child that is a subshell of its own (`( )`, `&`, a
    /// coprocess): forgets the parent's jobs, which bash's do not list.
    pub fn forget_jobs(&mut self) {
        self.jobs.list.retain(|j| !j.inherited);
        self.jobs.current = None;
        self.jobs.previous = None;
    }

    /// Waits for a foreground job to end or stop; its status.
    pub fn wait_job(&mut self, id: usize) -> Result<i32, Flow> {
        // A job of its own group can stop, and has the terminal, if the
        // shell has it, while it runs.
        let Some(job) = self.jobs.get(id) else { return Ok(0) };
        let (grouped, pgid) = (job.grouped, job.pgid);
        if grouped {
            self.give_terminal(pgid);
        }
        while let Some(job) = self.jobs.get(id) {
            if job.done() || job.stopped() {
                break;
            }
            let flags = if grouped { libc::WUNTRACED } else { 0 };
            // Any child: one of this job's, or a background one that ends
            // meanwhile.
            match sys::waitpid(-1, flags) {
                Ok(Some((pid, status))) => self.jobs.note(pid, status),
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
        if grouped {
            self.take_terminal();
        }
        self.reap_coprocs();
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
        self.report_signaled(&job, true);
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
        // Background jobs that ended meanwhile are news now, as in bash.
        self.notify_background();
        Ok(status)
    }

    /// Reports a job a signal ended, as bash does; SIGINT and SIGPIPE are
    /// no news. A non-interactive shell says where it is and lists the
    /// job's processes, but for SIGTERM, or a signal it traps itself,
    /// names the signal alone as an interactive one does, and only when
    /// it waited for the job in the `foreground`.
    fn report_signaled(&self, job: &Job, foreground: bool) {
        let Some(last) = job.procs.last().and_then(|p| p.status) else { return };
        if !libc::WIFSIGNALED(last) || self.in_subst {
            return;
        }
        let signal = libc::WTERMSIG(last);
        if matches!(signal, libc::SIGINT | libc::SIGPIPE) {
            return;
        }
        let text = if self.reports_by_line(job) {
            format!("{}: line {}: {}", self.source, self.report_line.max(1), job.listing())
        } else if foreground {
            describe_status(last) + "\n"
        } else {
            return;
        };
        let _ = sys::write_all(2, crate::text::encode(&text).as_slice());
    }

    /// Whether a non-interactive shell reports the end of `job` with its
    /// line and listing, as bash does: a signal ended it, but SIGINT,
    /// SIGPIPE, SIGTERM or one the shell traps.
    fn reports_by_line(&self, job: &Job) -> bool {
        let Some(last) = job.procs.last().and_then(|p| p.status) else { return false };
        let signal = libc::WTERMSIG(last);
        libc::WIFSIGNALED(last)
            && !self.in_subst
            && !self.interactive
            && !matches!(signal, libc::SIGINT | libc::SIGPIPE | libc::SIGTERM)
            && self.trap_command(signal as usize).is_none()
    }

    /// Whether background jobs are told of as they change, as an
    /// interactive shell tells of them: under `-c` with job control too,
    /// as in bash, but for the ends reported by line.
    fn tells(&self, job: &Job) -> bool {
        self.interactive
            || self.command_text
                && self.opts.monitor
                && !self.posix()
                && !(job.done() && self.reports_by_line(job))
    }

    /// Waits for the process `pid` alone, outside any job: a command
    /// substitution or a subshell.
    pub fn wait_pid(&mut self, pid: i32) -> i32 {
        loop {
            match sys::waitpid(pid, 0) {
                Ok(Some((_, status))) => return status_of(status),
                Ok(None) => {}
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => {}
                Err(_) => return self.jobs.reaped.remove(pid).map_or(127, status_of),
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
            self.jobs.note(pid, status);
        }
    }

    /// Reports the background jobs that ended or stopped, as an
    /// interactive shell does before its prompt, and forgets those that
    /// ended.
    /// Forgets the jobs `jobs` told of.
    pub fn forget_told(&mut self) {
        let told: Vec<usize> = self.jobs.list.iter().filter(|j| j.told).map(|j| j.id).collect();
        for id in told {
            self.jobs.remove(id);
        }
    }

    pub fn notify_jobs(&mut self) {
        self.forget_told();
        self.reap();
        // A script keeps a background job that ended without a signal, its
        // news untold, until `jobs` lists it or `wait` takes it, as bash
        // does; under `-c` it goes at once.
        let hold = !self.interactive && !self.command_text;
        let changed: Vec<usize> =
            self.jobs.list.iter().filter(|j| j.notify && !j.held).map(|j| j.id).collect();
        for id in changed {
            let Some(job) = self.jobs.get(id).cloned() else { continue };
            let signaled =
                job.procs.last().and_then(|p| p.status).is_some_and(|s| libc::WIFSIGNALED(s));
            if hold && job.done() && !signaled {
                if let Some(j) = self.jobs.get_mut(id) {
                    j.held = true;
                }
                continue;
            }
            if self.tells(&job) {
                self.print_job(&job, false);
            }
            if let Some(j) = self.jobs.get_mut(job.id) {
                j.notify = false;
            }
            if job.done() {
                if !self.interactive {
                    self.report_signaled(&job, false);
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
        if hold {
            self.drop_held();
        }
    }

    /// Past CHILD_MAX ended jobs held, forgets the oldest, keeping their
    /// statuses for `wait PID`: bash keeps every one, so that a script
    /// that starts jobs and never looks grows for ever.
    fn drop_held(&mut self) {
        let held: Vec<usize> = self.jobs.list.iter().filter(|j| j.held).map(|j| j.id).collect();
        let Some(excess) = held.len().checked_sub(child_max()) else { return };
        for id in held.into_iter().take(excess) {
            if let Some(job) = self.jobs.get(id).cloned() {
                for proc in &job.procs {
                    if let Some(status) = proc.status {
                        self.jobs.reaped.insert(proc.pid, status);
                    }
                }
            }
            self.jobs.remove(id);
        }
    }

    /// Forgets job `id`, which `wait` saw end. A shell that tells of jobs
    /// says how it ended first, as it would have before the next prompt.
    pub fn forget_waited(&mut self, id: usize) {
        if let Some(job) = self.jobs.get(id).cloned() {
            if !self.interactive {
                self.report_signaled(&job, false);
            }
            if job.notify && self.tells(&job) {
                self.print_job(&job, false);
            }
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
                "[{}]{mark} {:>5} {state:<24}{}{suffix}\n",
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
        let (pgid, grouped) = (job.pgid, job.grouped);
        let text = job.text.clone();
        self.jobs.make_current(id);
        if foreground {
            let _ = sys::write_all(1, crate::text::encode(&format!("{text}\n")).as_slice());
            if grouped {
                self.give_terminal(pgid);
            }
        } else {
            let _ =
                sys::write_all(1, crate::text::encode(&format!("[{id}]+ {text} &\n")).as_slice());
        }
        if let Some(job) = self.jobs.get(id) {
            job.signal(libc::SIGCONT);
        }
        if foreground {
            return self.wait_job(id);
        }
        Ok(0)
    }

    /// Sends SIGHUP to every job (and SIGCONT to the stopped ones), as a
    /// shell that hangs up does.
    pub fn hang_up_jobs(&mut self) {
        for job in &self.jobs.list {
            if job.nohup {
                continue;
            }
            job.signal(libc::SIGHUP);
            if job.stopped() {
                job.signal(libc::SIGCONT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_last_child_max_statuses_are_kept() {
        let mut reaped = Reaped::default();
        let limit = child_max();
        for pid in 1..=(limit as i32 * 2) {
            reaped.insert(pid, 0);
        }
        assert!(reaped.len() <= limit + limit / 8);
        assert_eq!(reaped.remove(limit as i32 * 2), Some(0));
        assert_eq!(reaped.remove(1), None);
        assert!(reaped.oldest().is_some_and(|(pid, _)| pid > limit as i32 / 2));
    }

    #[test]
    fn a_job_is_listed_as_bash_lists_it() {
        let ended = |pid, status, text: &str| Proc {
            pid,
            status: Some(status),
            stopped: false,
            text: text.into(),
        };
        let job = Job {
            id: 1,
            pgid: 7,
            procs: vec![
                ended(7, 3 << 8, "a"),
                ended(8, 3 << 8, "b"),
                ended(9, libc::SIGSEGV | 0x80, "c"),
            ],
            text: "a | b | c".into(),
            notify: false,
            nohup: false,
            background: false,
            grouped: false,
            inherited: false,
            held: false,
            told: false,
        };
        assert_eq!(
            job.listing(),
            "    7 Exit 3                  a\n\
             \x20        8                       | b\n\
             \x20        9 Segmentation fault      (core dumped) | c\n"
        );
    }
}
