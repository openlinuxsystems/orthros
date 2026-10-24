// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Jobs: `jobs`, `fg`, `bg`, `wait`, `kill`, `disown` and `suspend`.

use super::{flags, out, usage};
use crate::jobs::status_of;
use crate::shell::{Flow, Res, Shell};
use crate::{sys, trap};

/// The job `spec` names, or the current one; an error printed if none.
fn job_of(shell: &Shell, name: &str, spec: Option<&str>) -> Option<usize> {
    let spec = spec.unwrap_or("%%");
    match shell.jobs.find(spec) {
        Ok(id) => Some(id),
        Err(message) => {
            let message = if spec == "%%" { "current: no such job".to_string() } else { message };
            shell.error(&format!("{name}: {message}"));
            None
        }
    }
}

pub fn jobs(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "jobs", &args[1..], "lnprsx", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    shell.reap();
    let ids: Vec<usize> = if flags.rest.is_empty() {
        shell.jobs.list.iter().map(|j| j.id).collect()
    } else {
        let mut ids = Vec::new();
        for spec in &flags.rest {
            match shell.jobs.find(spec) {
                Ok(id) => ids.push(id),
                Err(message) => {
                    shell.error(&format!("jobs: {message}"));
                    return Ok(1);
                }
            }
        }
        ids
    };
    let mut text = String::new();
    for id in &ids {
        let Some(job) = shell.jobs.get(*id).cloned() else { continue };
        if job.told
            || flags.has('r') && (job.stopped() || job.done())
            || flags.has('s') && !job.stopped()
        {
            continue;
        }
        if flags.has('n') && !job.notify {
            continue;
        }
        if flags.has('p') {
            text.push_str(&format!("{}\n", job.pgid));
            continue;
        }
        let mark = if shell.jobs.current == Some(job.id) {
            '+'
        } else if shell.jobs.previous == Some(job.id) {
            '-'
        } else {
            ' '
        };
        let state = job.state();
        let suffix = if state == "Running" { " &" } else { "" };
        if flags.has('l') {
            text.push_str(&format!(
                "[{}]{mark} {:>5} {state:<24}{}{suffix}\n",
                job.id, job.pgid, job.text
            ));
        } else {
            text.push_str(&format!("[{}]{mark}  {state:<24}{}{suffix}\n", job.id, job.text));
        }
    }
    let written = out(shell, "jobs", &text);
    // Jobs listed as done are forgotten before the next command, as in
    // bash: `jobs; kill %1` still finds the first.
    for id in ids {
        if let Some(job) = shell.jobs.get_mut(id) {
            job.told = job.done();
            job.notify = false;
        }
    }
    Ok(i32::from(!written))
}

fn job_control(shell: &Shell, name: &str) -> bool {
    if !shell.opts.monitor {
        shell.error(&format!("{name}: no job control"));
        return false;
    }
    true
}

pub fn fg(shell: &mut Shell, args: &[String]) -> Res {
    if !job_control(shell, "fg") {
        return Ok(1);
    }
    shell.reap();
    let Some(id) = job_of(shell, "fg", args.get(1).map(String::as_str)) else { return Ok(1) };
    shell.continue_job(id, true)
}

pub fn bg(shell: &mut Shell, args: &[String]) -> Res {
    if !job_control(shell, "bg") {
        return Ok(1);
    }
    // A job stopped since the shell last looked is stopped for `bg`.
    shell.reap();
    let specs: Vec<Option<&str>> = if args.len() < 2 {
        vec![None]
    } else {
        args[1..].iter().map(|s| Some(s.as_str())).collect()
    };
    let mut status = 0;
    for spec in specs {
        let Some(id) = job_of(shell, "bg", spec) else {
            status = 1;
            continue;
        };
        if shell.jobs.get(id).is_some_and(|j| !j.stopped()) {
            shell.error(&format!("bg: job {id} already in background"));
            continue;
        }
        shell.continue_job(id, false)?;
    }
    Ok(status)
}

pub fn disown(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "disown", &args[1..], "ahr", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let ids: Vec<usize> = if flags.has('a') || flags.has('r') && flags.rest.is_empty() {
        shell
            .jobs
            .list
            .iter()
            .filter(|j| !flags.has('r') || !j.stopped() && !j.done())
            .map(|j| j.id)
            .collect()
    } else if flags.rest.is_empty() {
        match job_of(shell, "disown", None) {
            Some(id) => vec![id],
            None => return Ok(1),
        }
    } else {
        let mut ids = Vec::new();
        for spec in &flags.rest {
            // A process number names its job too.
            let found = match spec.parse::<i32>() {
                Ok(pid) => shell
                    .jobs
                    .list
                    .iter()
                    .find(|j| j.procs.iter().any(|p| p.pid == pid))
                    .map(|j| j.id),
                Err(_) => shell.jobs.find(spec).ok(),
            };
            match found {
                Some(id) => ids.push(id),
                None => {
                    shell.error(&format!("disown: {spec}: no such job"));
                    return Ok(1);
                }
            }
        }
        ids
    };
    for id in ids {
        if flags.has('h') {
            if let Some(job) = shell.jobs.get_mut(id) {
                job.nohup = true;
            }
        } else {
            shell.jobs.remove(id);
        }
    }
    Ok(0)
}

pub fn suspend(shell: &mut Shell, args: &[String]) -> Res {
    let force = args.get(1).is_some_and(|a| a == "-f");
    if shell.login && !force {
        shell.error("suspend: cannot suspend a login shell");
        return Ok(1);
    }
    trap::set(libc::SIGTSTP, trap::Disposition::Default);
    let _ = sys::kill(0, libc::SIGSTOP);
    shell.apply_disposition(libc::SIGTSTP);
    Ok(0)
}

/// What `wait` waits for.
enum Target {
    /// A process, or the last of a job.
    Pid(i32),
    Job(usize),
}

pub fn wait(shell: &mut Shell, args: &[String]) -> Res {
    // A job a signal ended is reported at wait's line, as bash does.
    let report_line = std::mem::replace(&mut shell.report_line, shell.lineno);
    let result = wait_for(shell, args);
    shell.report_line = report_line;
    shell.reap_coprocs();
    result
}

fn wait_for(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "wait", &args[1..], "fnp:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let pid_var = flags.value('p').map(str::to_string);
    if flags.has('n') {
        // An operand that is not a job of this shell's is reported and
        // left out, as in bash; with none left, the status is 127.
        let mut targets = Vec::new();
        for spec in &flags.rest {
            let target = if spec.starts_with('%') {
                shell
                    .jobs
                    .find(spec)
                    .ok()
                    .filter(|id| shell.jobs.get(*id).is_some_and(|j| !j.inherited))
                    .map(Target::Job)
            } else {
                spec.parse::<i32>()
                    .ok()
                    .filter(|pid| {
                        *pid > 0
                            && shell
                                .jobs
                                .list
                                .iter()
                                .any(|j| !j.inherited && j.procs.iter().any(|p| p.pid == *pid))
                    })
                    .map(Target::Pid)
            };
            match target {
                Some(target) => targets.push(target),
                None => shell.error(&format!("wait: {spec}: no such job")),
            }
        }
        if targets.is_empty() && !flags.rest.is_empty() {
            return Ok(127);
        }
        return wait_next(shell, &targets, pid_var.as_deref());
    }
    if flags.rest.is_empty() {
        // Every background job.
        loop {
            shell.reap_into_jobs();
            if shell.jobs.list.iter().all(|j| j.inherited || j.done() || j.stopped()) {
                break;
            }
            if let Some(signal) = wait_any(shell)? {
                return Ok(128 + signal);
            }
        }
        // Those a script held from before stay held, as in bash.
        let done: Vec<usize> = shell
            .jobs
            .list
            .iter()
            .filter(|j| !j.inherited && !j.held && j.done())
            .map(|j| j.id)
            .collect();
        for id in done {
            shell.forget_waited(id);
        }
        shell.jobs.reaped.clear();
        return Ok(0);
    }
    // Each operand in turn; the status is the last one's, as in bash.
    let mut status = 0;
    for spec in &flags.rest {
        let target = if spec.starts_with('%') {
            match shell.jobs.find(spec) {
                Ok(id) => Target::Job(id),
                Err(message) => {
                    shell.error(&format!("wait: {message}"));
                    status = 127;
                    continue;
                }
            }
        } else {
            match spec.parse::<i32>() {
                Ok(pid) if pid > 0 => Target::Pid(pid),
                _ => {
                    shell.error(&format!("wait: `{spec}': not a pid or valid job spec"));
                    status = 1;
                    continue;
                }
            }
        };
        match wait_target(shell, target)? {
            Ok(done) => status = done,
            // A trapped signal ends the wait at once.
            Err(signal) => return Ok(128 + signal),
        }
    }
    Ok(status)
}

/// Waits for one job or process: its status, or the trapped signal that
/// came first.
fn wait_target(shell: &mut Shell, target: Target) -> Result<Result<i32, i32>, Flow> {
    loop {
        shell.reap_into_jobs();
        let finished = match target {
            Target::Job(id) => match shell.jobs.get(id) {
                Some(job) if job.inherited => {
                    let pid = job.procs.last().map_or(0, |p| p.pid);
                    shell.error(&format!("wait: pid {pid} is not a child of this shell"));
                    Some(127)
                }
                Some(job) if job.done() => {
                    let status = job.status(shell.opts.pipefail);
                    shell.forget_waited(id);
                    Some(status)
                }
                Some(_) => None,
                None => Some(127),
            },
            Target::Pid(pid) => {
                let job = shell
                    .jobs
                    .list
                    .iter()
                    .find(|j| !j.inherited && j.procs.iter().any(|p| p.pid == pid))
                    .cloned();
                match job {
                    Some(job) => {
                        let proc = job.procs.iter().find(|p| p.pid == pid);
                        match proc.and_then(|p| p.status) {
                            Some(raw) => {
                                if job.done() {
                                    shell.forget_waited(job.id);
                                }
                                Some(status_of(raw))
                            }
                            None => None,
                        }
                    }
                    None => match shell.jobs.reaped.remove(pid) {
                        Some(raw) => Some(status_of(raw)),
                        None => {
                            shell.error(&format!("wait: pid {pid} is not a child of this shell"));
                            Some(127)
                        }
                    },
                }
            }
        };
        if let Some(status) = finished {
            return Ok(Ok(status));
        }
        if let Some(signal) = wait_any(shell)? {
            return Ok(Err(signal));
        }
    }
}

fn wait_next(shell: &mut Shell, targets: &[Target], pid_var: Option<&str>) -> Res {
    loop {
        shell.reap_into_jobs();
        let wanted = |id: usize, shell: &Shell| -> bool {
            targets.is_empty()
                || targets.iter().any(|t| match t {
                    Target::Job(j) => *j == id,
                    Target::Pid(pid) => {
                        shell.jobs.get(id).is_some_and(|j| j.procs.iter().any(|p| p.pid == *pid))
                    }
                })
        };
        let done = shell
            .jobs
            .list
            .iter()
            .find(|j| !j.inherited && j.done() && wanted(j.id, shell))
            .cloned();
        if let Some(job) = done {
            let status = job.status(shell.opts.pipefail);
            if let Some(var) = pid_var {
                let pid = job.procs.last().map_or(0, |p| p.pid);
                let _ = shell.assign(var, pid.to_string(), false);
            }
            shell.forget_waited(job.id);
            return Ok(status);
        }
        // A background child that ended before it was looked at.
        if targets.is_empty()
            && let Some((pid, raw)) = shell.jobs.reaped.oldest()
        {
            shell.jobs.reaped.remove(pid);
            if let Some(var) = pid_var {
                let _ = shell.assign(var, pid.to_string(), false);
            }
            return Ok(status_of(raw));
        }
        if !shell.jobs.list.iter().any(|j| !j.inherited && !j.done() && wanted(j.id, shell)) {
            return Ok(127);
        }
        if let Some(signal) = wait_any(shell)? {
            return Ok(128 + signal);
        }
    }
}

impl Shell {
    /// Notes every child that ended, without waiting.
    fn reap_into_jobs(&mut self) {
        while let Ok(Some((pid, status))) = sys::waitpid(-1, libc::WNOHANG) {
            self.jobs.note(pid, status);
        }
    }
}

/// Waits for any child to change: the signal that interrupted the wait,
/// if a trap is set for it.
fn wait_any(shell: &mut Shell) -> Result<Option<i32>, Flow> {
    match sys::waitpid(-1, 0) {
        Ok(Some((pid, status))) => {
            shell.jobs.note(pid, status);
            Ok(None)
        }
        Ok(None) => Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::EINTR) => {
            if let Some(signal) = trap::first_pending()
                && (shell.trap_command(signal as usize).is_some()
                    || shell.interactive && signal == libc::SIGINT)
            {
                return Ok(Some(signal));
            }
            // A signal nobody traps: forget it, wait on.
            if let Some(signal) = trap::first_pending()
                && shell.trap_command(signal as usize).is_none()
                && !(shell.interactive && signal == libc::SIGINT)
            {
                trap::take_one(signal);
            }
            Ok(None)
        }
        Err(_) => {
            // No children: every job left has ended.
            for job in shell.jobs.list.iter_mut().filter(|j| !j.inherited) {
                for proc in &mut job.procs {
                    proc.status.get_or_insert(0);
                }
            }
            Ok(None)
        }
    }
}

/// A signal given to `kill`: by name or number.
fn signal_arg(text: &str) -> Option<i32> {
    if let Ok(n) = text.parse::<i32>() {
        return (0..=trap::MAX_SIGNAL as i32).contains(&n).then_some(n);
    }
    trap::parse(text, false).map(|n| n as i32)
}

pub fn kill(shell: &mut Shell, args: &[String]) -> Res {
    let mut signal = libc::SIGTERM;
    let mut i = 1;
    let mut list = false;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "-l" | "-L" => {
                list = true;
                i += 1;
            }
            "-s" | "-n" => {
                let Some(name) = args.get(i + 1) else {
                    return usage(shell, "kill", &format!("{arg}: option requires an argument"));
                };
                match signal_arg(name) {
                    Some(n) => signal = n,
                    None => {
                        shell.error(&format!("kill: {name}: invalid signal specification"));
                        return Ok(1);
                    }
                }
                i += 2;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                match signal_arg(&s[1..]) {
                    Some(n) => signal = n,
                    None => {
                        shell.error(&format!("kill: {}: invalid signal specification", &s[1..]));
                        return Ok(1);
                    }
                }
                i += 1;
            }
            _ => break,
        }
    }
    if list {
        let rest = &args[i..];
        if rest.is_empty() {
            return Ok(i32::from(!out(shell, "kill", &trap::table())));
        }
        // Each answer goes out as it comes, between the errors, as in bash.
        let mut status = 0;
        for arg in rest {
            let answer = match arg.parse::<i32>() {
                // An exit status of a signal's death names the signal; 0
                // is EXIT, as for trap.
                Ok(0) => Some("EXIT".to_string()),
                Ok(n) => trap::name(if n > 128 { n - 128 } else { n }),
                Err(_) if arg.eq_ignore_ascii_case("EXIT") => Some("0".to_string()),
                Err(_) => trap::parse(arg, false).map(|n| n.to_string()),
            };
            match answer {
                Some(answer) => {
                    if !out(shell, "kill", &format!("{answer}\n")) {
                        return Ok(1);
                    }
                }
                None => {
                    shell.error(&format!("kill: {arg}: invalid signal specification"));
                    status = 1;
                }
            }
        }
        return Ok(status);
    }
    let targets = &args[i..];
    if targets.is_empty() {
        return usage(shell, "kill", "");
    }
    let mut status = 0;
    for target in targets {
        let pids: Vec<i32> = if target.starts_with('%') {
            match shell.jobs.find(target) {
                Ok(id) => {
                    let job = shell.jobs.get(id).cloned();
                    match job {
                        // One that ended takes the signal as done, as in
                        // bash.
                        Some(job) if !job.done() => shell.job_targets(&job),
                        _ => Vec::new(),
                    }
                }
                Err(message) => {
                    shell.error(&format!("kill: {message}"));
                    status = 1;
                    continue;
                }
            }
        } else {
            match target.parse::<i32>() {
                Ok(pid) => vec![pid],
                Err(_) => {
                    shell.error(&format!("kill: {target}: arguments must be process or job IDs"));
                    status = 1;
                    continue;
                }
            }
        };
        for pid in pids {
            if let Err(error) = sys::kill(pid, signal) {
                shell.error(&format!("kill: ({pid}) - {}", sys::describe(&error)));
                status = 1;
            } else if signal == libc::SIGHUP || signal == libc::SIGTERM || signal == libc::SIGKILL {
                // A stopped job does not die until it is continued.
                if let Some(job) = shell.jobs.list.iter().find(|j| {
                    j.stopped() && (j.pgid == -pid || j.procs.iter().any(|p| p.pid == pid))
                }) {
                    if pid < 0 {
                        shell.signal_job(job, libc::SIGCONT);
                    } else {
                        let _ = sys::kill(pid, libc::SIGCONT);
                    }
                }
            }
        }
    }
    Ok(status)
}
