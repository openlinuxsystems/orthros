// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running the syntax tree.
//!
//! Lists, pipelines and compound commands run here; a simple command is
//! expanded, then run as a special builtin, a function, a builtin or a
//! program on `PATH`, in that order in POSIX mode (functions come first
//! otherwise, as in bash). Pipelines, subshells, command substitutions and
//! `&` fork; the shell runs everything else itself.

use std::rc::Rc;
use std::time::Instant;

use crate::ast::*;
use crate::builtins::{self, DeclArg};
use crate::pattern::Pattern;
use crate::shell::{Flow, Frame, Res, Shell, temp_var};
use crate::vars::attr;
use crate::{show, sys, text, trap};

/// How deep functions may call each other.
pub const MAX_NESTING: usize = 2000;

/// A command's assignments, expanded.
pub enum Assigned {
    Scalar { name: String, index: Option<String>, append: bool, value: String },
    Array { name: String, append: bool, elements: Vec<(Option<String>, String)> },
}

impl Shell {
    pub fn run_program(&mut self, program: &Program) -> Res {
        let mut status = 0;
        for command in &program.commands {
            status = self.run_complete(command)?;
        }
        Ok(status)
    }

    pub fn run_complete(&mut self, command: &CompleteCommand) -> Res {
        let mut status = 0;
        for (and_or, sep) in &command.items {
            status = match sep {
                Sep::Async => self.run_async(and_or)?,
                Sep::Seq => self.run_and_or(and_or)?,
            };
            self.run_pending_traps()?;
        }
        Ok(status)
    }

    pub fn run_and_or(&mut self, list: &AndOr) -> Res {
        let count = list.rest.len();
        let mut status = self.run_pipeline_in(&list.first, count > 0)?;
        for (i, (op, pipeline)) in list.rest.iter().enumerate() {
            let go = match op {
                AndOrOp::And => status == 0,
                AndOrOp::Or => status != 0,
            };
            if go {
                status = self.run_pipeline_in(pipeline, i + 1 < count)?;
            }
        }
        Ok(status)
    }

    /// Runs a pipeline of an and-or list; `set -e` and the ERR trap apply
    /// unless more of the list follows, or the pipeline is negated.
    fn run_pipeline_in(&mut self, pipeline: &Pipeline, more: bool) -> Res {
        let off = more || pipeline.bang;
        if off {
            self.errexit_off += 1;
        }
        let result = self.run_pipeline(pipeline);
        if off {
            self.errexit_off -= 1;
        }
        let status = result?;
        self.status = status;
        if status != 0 && !off {
            self.failed(status)?;
        }
        Ok(status)
    }

    /// A command failed where `set -e` and the ERR trap apply.
    pub fn failed(&mut self, status: i32) -> Result<(), Flow> {
        if self.errexit_off > 0 {
            return Ok(());
        }
        let in_function = self.frames.iter().any(|f| f.name != "source");
        if self.trap_command(trap::ERR).is_some() && (!in_function || self.opts.errtrace) {
            self.run_trap(trap::ERR)?;
            self.status = status;
        }
        if self.opts.errexit {
            return Err(Flow::Exit(status));
        }
        Ok(())
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline) -> Res {
        let started = pipeline.time.map(|_| (Instant::now(), sys::times()));
        let mut status = if pipeline.commands.len() == 1 {
            let status = self.run_command(&pipeline.commands[0], false)?;
            self.pipestatus = vec![status];
            status
        } else {
            self.run_multi(pipeline)?
        };
        if pipeline.bang {
            status = i32::from(status == 0);
        }
        if let Some(started) = started {
            self.report_time(started, pipeline.time == Some(true));
        }
        Ok(status)
    }

    /// `time`'s report on standard error.
    fn report_time(&self, (start, before): (Instant, (f64, f64, f64, f64)), posix: bool) {
        let real = start.elapsed().as_secs_f64();
        let after = sys::times();
        let user = (after.0 - before.0) + (after.2 - before.2);
        let system = (after.1 - before.1) + (after.3 - before.3);
        let text = if posix {
            format!("real {real:.2}\nuser {user:.2}\nsys {system:.2}\n")
        } else if let Some(format) = self.get("TIMEFORMAT") {
            let mut out = String::new();
            let mut chars = format.chars().peekable();
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                let mut digits = 3;
                if let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                    digits = d.min(3) as usize;
                    chars.next();
                }
                let long = chars.peek() == Some(&'l');
                if long {
                    chars.next();
                }
                let value = match chars.next() {
                    Some('R') => real,
                    Some('U') => user,
                    Some('S') => system,
                    Some('P') => {
                        if real > 0.0 {
                            (user + system) / real * 100.0
                        } else {
                            0.0
                        }
                    }
                    Some('%') => {
                        out.push('%');
                        continue;
                    }
                    _ => continue,
                };
                if long {
                    let minutes = (value / 60.0).floor();
                    out.push_str(&format!("{minutes}m{:.digits$}s", value - minutes * 60.0));
                } else {
                    out.push_str(&format!("{value:.digits$}"));
                }
            }
            out.push('\n');
            out
        } else {
            let mmss = |t: f64| {
                let minutes = (t / 60.0).floor();
                format!("{}m{:.3}s", minutes as i64, t - minutes * 60.0)
            };
            format!("\nreal\t{}\nuser\t{}\nsys\t{}\n", mmss(real), mmss(user), mmss(system))
        };
        let _ = sys::write_all(2, &text::encode(&text));
    }

    /// A pipeline of two commands or more: each in a child, joined by
    /// pipes. With `lastpipe` and no job control, the last runs in the
    /// shell itself.
    fn run_multi(&mut self, pipeline: &Pipeline) -> Res {
        let count = pipeline.commands.len();
        let lastpipe = self.shopt("lastpipe") && !self.opts.monitor;
        let mut pids = Vec::new();
        let mut pgid = 0;
        let mut input: Option<i32> = None;
        let mut last_status = None;
        for (i, cmd) in pipeline.commands.iter().enumerate() {
            let last = i + 1 == count;
            if last && lastpipe {
                let saved = sys::dup_high(0).ok();
                if let Some(fd) = input.take() {
                    let _ = sys::dup2(fd, 0);
                    sys::close(fd);
                }
                let result = self.run_command(cmd, false);
                if let Some(saved) = saved {
                    let _ = sys::dup2(saved, 0);
                    sys::close(saved);
                }
                last_status = Some(result?);
                break;
            }
            let (next_read, write) = if last {
                (None, None)
            } else {
                match sys::pipe() {
                    Ok((r, w)) => (Some(r), Some(w)),
                    Err(error) => {
                        self.error(&format!("pipe error: {}", sys::describe(&error)));
                        return Ok(1);
                    }
                }
            };
            let pid = self.fork_child(pgid, false)?;
            if pid == 0 {
                if let Some(fd) = input {
                    let _ = sys::dup2(fd, 0);
                    sys::close(fd);
                }
                if let Some(w) = write {
                    let _ = sys::dup2(w, 1);
                    if pipeline.pipe_stderr[i] {
                        let _ = sys::dup2(w, 2);
                    }
                    sys::close(w);
                }
                if let Some(r) = next_read {
                    sys::close(r);
                }
                let result = self.run_command(cmd, true);
                self.exit_child(result);
            }
            if pgid == 0 {
                pgid = pid;
            }
            pids.push(pid);
            if let Some(fd) = input {
                sys::close(fd);
            }
            if let Some(w) = write {
                sys::close(w);
            }
            input = next_read;
        }
        let text = show::pipeline_line(pipeline);
        let id = self.add_job(pgid, &pids, text, false);
        let status = self.wait_job(id)?;
        if let Some(last_status) = last_status {
            self.pipestatus.push(last_status);
            if self.opts.pipefail && last_status == 0 {
                return Ok(self.pipestatus.iter().rev().find(|s| **s != 0).copied().unwrap_or(0));
            }
            return Ok(last_status);
        }
        Ok(status)
    }

    /// `list &`: in a child, which the shell does not wait for.
    fn run_async(&mut self, list: &AndOr) -> Res {
        let pid = self.fork_child(0, true)?;
        if pid == 0 {
            if !self.opts.monitor {
                // Without job control, Ctrl-C is not for background jobs,
                // and their input is empty unless redirected.
                trap::set(libc::SIGINT, trap::Disposition::Ignore);
                trap::set(libc::SIGQUIT, trap::Disposition::Ignore);
                if let Ok(null) = sys::open("/dev/null", libc::O_RDONLY, 0) {
                    let _ = sys::dup2(null, 0);
                    sys::close(null);
                }
            }
            let result = self.run_and_or(list);
            self.exit_child(result);
        }
        self.last_bg = Some(pid);
        let id = self.add_job(pid, &[pid], show::and_or_line(list), true);
        if self.interactive {
            let _ = sys::write_all(2, format!("[{id}] {pid}\n").as_bytes());
        }
        Ok(0)
    }

    /// Forks a child of the shell. The child is a subshell: it forgets the
    /// jobs and traps, and with job control joins (or starts) process
    /// group `pgid`, taking the terminal unless `background`. Returns the
    /// child's number, or 0 in the child.
    pub fn fork_child(&mut self, pgid: i32, background: bool) -> Result<i32, Flow> {
        let pid = match sys::fork() {
            Ok(pid) => pid,
            Err(error) => {
                return Err(self.fatal(&format!("fork: {}", sys::describe(&error)), 1));
            }
        };
        let monitor = self.opts.monitor && self.tty.is_some();
        if pid != 0 {
            if monitor {
                let _ = sys::setpgid(pid, if pgid == 0 { pid } else { pgid });
            }
            return Ok(pid);
        }
        // The child.
        if monitor {
            let own = sys::getpid();
            let group = if pgid == 0 { own } else { pgid };
            let _ = sys::setpgid(0, group);
            if !background && let Some(tty) = self.tty {
                let _ = sys::tcsetpgrp(tty, group);
            }
            for signal in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                trap::set(signal, trap::Disposition::Default);
            }
        }
        self.become_subshell();
        Ok(0)
    }

    /// What a child does to stop being the parent shell.
    pub fn become_subshell(&mut self) {
        self.subshell += 1;
        self.jobs = crate::jobs::Jobs::default();
        self.opts.monitor = false;
        if self.interactive {
            self.interactive = false;
            for signal in [
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGTSTP,
                libc::SIGTTIN,
                libc::SIGTTOU,
                libc::SIGHUP,
                libc::SIGWINCH,
            ] {
                if self.trap_command(signal as usize).is_none() {
                    trap::set(signal, trap::Disposition::Default);
                }
            }
        }
        self.tty = None;
        // Traps with commands go back to the default; ignored signals stay
        // ignored.
        for (slot, command) in self.traps.iter_mut().enumerate().take(trap::MAX_SIGNAL + 1).skip(1)
        {
            if command.as_deref().is_some_and(|t| !t.is_empty()) {
                *command = None;
                trap::set(slot as i32, trap::Disposition::Default);
            }
        }
        self.traps[trap::EXIT] = None;
        self.traps[trap::RETURN] = None;
        if !self.opts.errtrace {
            self.traps[trap::ERR] = None;
        }
        self.history.disable();
    }

    /// Ends a child with its command's result, running its EXIT trap.
    pub fn exit_child(&mut self, result: Res) -> ! {
        let status = self.flow_status(result);
        let status = self.run_exit_trap(status);
        sys::exit_now(status)
    }

    /// The status a command's result leaves.
    pub fn flow_status(&self, result: Res) -> i32 {
        match result {
            Ok(status) => status,
            Err(Flow::Exit(status) | Flow::Fatal(status) | Flow::Return(status)) => status,
            Err(Flow::Break(_) | Flow::Continue(_)) => self.status,
            Err(Flow::Interrupt) => 130,
        }
    }

    /// Runs the EXIT trap, once; the status the shell exits with.
    pub fn run_exit_trap(&mut self, status: i32) -> i32 {
        self.status = status;
        let Some(command) = self.traps[trap::EXIT].take().filter(|c| !c.is_empty()) else {
            return status;
        };
        match self.eval_text(&command, "trap") {
            Err(Flow::Exit(code)) => code,
            _ => status,
        }
    }

    // Commands.

    /// Runs a command; `last` when nothing follows it in this process, so
    /// that a program can replace the shell instead of being forked.
    pub fn run_command(&mut self, command: &Command, last: bool) -> Res {
        match command {
            Command::Simple(simple) => self.run_simple(simple, last),
            Command::Compound(compound, redirects) => {
                if redirects.is_empty() {
                    return self.run_compound(compound);
                }
                let saved = match self.redirect(redirects, true) {
                    Ok(saved) => saved,
                    Err(Some(flow)) => return Err(flow),
                    Err(None) => return Ok(1),
                };
                let result = self.run_compound(compound);
                self.restore(saved);
                result
            }
            Command::Function(def) => {
                if self.posix() && builtins::is_special(&def.name) {
                    return Err(self.fatal(&format!("`{}': is a special builtin", def.name), 2));
                }
                if self.functions.contains_key(&def.name)
                    && self.readonly_functions.contains(&def.name)
                {
                    self.error(&format!("{}: readonly function", def.name));
                    return Ok(1);
                }
                self.functions.insert(def.name.clone(), def.clone());
                Ok(0)
            }
        }
    }

    fn run_compound(&mut self, compound: &CompoundCommand) -> Res {
        match compound {
            CompoundCommand::Brace(program) => self.run_program(program),
            CompoundCommand::Subshell(program) => {
                let pid = self.fork_child(0, false)?;
                if pid == 0 {
                    let result = self.run_program(program);
                    self.exit_child(result);
                }
                Ok(self.wait_child(pid))
            }
            CompoundCommand::If { arms, otherwise } => {
                for (cond, then) in arms {
                    if self.condition(cond)? == 0 {
                        return self.run_program(then);
                    }
                }
                match otherwise {
                    Some(otherwise) => self.run_program(otherwise),
                    None => Ok(0),
                }
            }
            CompoundCommand::While { cond, body } => self.run_loop(cond, body, true),
            CompoundCommand::Until { cond, body } => self.run_loop(cond, body, false),
            CompoundCommand::For { var, words, body, line } => {
                self.lineno = *line;
                let items = match words {
                    Some(words) => self.expand_words(words)?,
                    None => self.args.clone(),
                };
                if self.opts.xtrace && words.is_some() {
                    self.trace(&format!(
                        "for {var} in {}",
                        items.iter().map(|i| crate::expand::quote(i)).collect::<Vec<_>>().join(" ")
                    ));
                }
                self.loops += 1;
                let mut status = 0;
                let result = (|| {
                    for item in items {
                        if let Err(message) = self.assign(var, item, false) {
                            return Err(self.fatal(&message, 1));
                        }
                        match self.run_program(body) {
                            Ok(s) => status = s,
                            Err(Flow::Break(n)) => {
                                if n > 1 {
                                    return Err(Flow::Break(n - 1));
                                }
                                return Ok(self.status);
                            }
                            Err(Flow::Continue(n)) => {
                                if n > 1 {
                                    return Err(Flow::Continue(n - 1));
                                }
                                status = self.status;
                            }
                            Err(flow) => return Err(flow),
                        }
                    }
                    Ok(status)
                })();
                self.loops -= 1;
                result
            }
            CompoundCommand::ArithFor { init, cond, step, body, line } => {
                self.lineno = *line;
                let init = self.expand_quoted(init)?;
                self.arith(&init)?;
                self.loops += 1;
                let mut status = 0;
                let result = (|| {
                    loop {
                        let test = self.expand_quoted(cond)?;
                        if !test.trim().is_empty() && self.arith(&test)? == 0 {
                            return Ok(status);
                        }
                        match self.run_program(body) {
                            Ok(s) => status = s,
                            Err(Flow::Break(n)) => {
                                if n > 1 {
                                    return Err(Flow::Break(n - 1));
                                }
                                return Ok(self.status);
                            }
                            Err(Flow::Continue(n)) => {
                                if n > 1 {
                                    return Err(Flow::Continue(n - 1));
                                }
                            }
                            Err(flow) => return Err(flow),
                        }
                        let next = self.expand_quoted(step)?;
                        self.arith(&next)?;
                    }
                })();
                self.loops -= 1;
                result
            }
            CompoundCommand::Select { var, words, body, line } => {
                self.lineno = *line;
                let items = match words {
                    Some(words) => self.expand_words(words)?,
                    None => self.args.clone(),
                };
                self.run_select(var, &items, body)
            }
            CompoundCommand::Case { word, arms, line } => {
                self.lineno = *line;
                let subject = self.expand_string(word)?;
                if self.opts.xtrace {
                    self.trace(&format!("case {} in", crate::expand::quote(&subject)));
                }
                let mut status = 0;
                let mut i = 0;
                let mut fall = false;
                while i < arms.len() {
                    let arm = &arms[i];
                    let mut matched = fall;
                    if !matched {
                        for pattern in &arm.patterns {
                            let text = self.expand_pattern(pattern)?;
                            if Pattern::new(&text, self.pattern_options()).matches(&subject) {
                                matched = true;
                                break;
                            }
                        }
                    }
                    fall = false;
                    if matched {
                        status = self.run_program(&arm.body)?;
                        match arm.end {
                            CaseEnd::Break => return Ok(status),
                            CaseEnd::FallThrough => fall = true,
                            CaseEnd::Continue => {}
                        }
                    }
                    i += 1;
                }
                Ok(status)
            }
            CompoundCommand::Cond(expr, line) => {
                self.lineno = *line;
                if self.opts.xtrace {
                    let mut text = String::from("[[ ");
                    text.push_str(
                        &show::one_line(&Command::Compound(Box::new(compound.clone()), Vec::new()))
                            [3..],
                    );
                    self.trace(&text);
                }
                let status = self.eval_cond(expr)?;
                Ok(status)
            }
            CompoundCommand::Arith(word, line) => {
                self.lineno = *line;
                let text = self.expand_quoted(word)?;
                if self.opts.xtrace {
                    self.trace(&format!("(( {} ))", text.trim()));
                }
                Ok(i32::from(self.arith(&text)? == 0))
            }
        }
    }

    /// Runs the condition of `if`, `while` or `until`, where `set -e`
    /// does not apply.
    fn condition(&mut self, cond: &Program) -> Res {
        self.errexit_off += 1;
        let result = self.run_program(cond);
        self.errexit_off -= 1;
        result
    }

    fn run_loop(&mut self, cond: &Program, body: &Program, while_true: bool) -> Res {
        self.loops += 1;
        let mut status = 0;
        let result = (|| {
            loop {
                let test = match self.condition(cond) {
                    Ok(s) => s,
                    Err(Flow::Break(n)) => {
                        if n > 1 {
                            return Err(Flow::Break(n - 1));
                        }
                        return Ok(0);
                    }
                    Err(Flow::Continue(n)) => {
                        if n > 1 {
                            return Err(Flow::Continue(n - 1));
                        }
                        continue;
                    }
                    Err(flow) => return Err(flow),
                };
                if (test == 0) != while_true {
                    return Ok(status);
                }
                match self.run_program(body) {
                    Ok(s) => status = s,
                    Err(Flow::Break(n)) => {
                        if n > 1 {
                            return Err(Flow::Break(n - 1));
                        }
                        return Ok(self.status);
                    }
                    Err(Flow::Continue(n)) => {
                        if n > 1 {
                            return Err(Flow::Continue(n - 1));
                        }
                        status = self.status;
                    }
                    Err(flow) => return Err(flow),
                }
                self.check_interrupt()?;
            }
        })();
        self.loops -= 1;
        result
    }

    /// `select`: a numbered menu on standard error, and a line read from
    /// standard input, until `break` or the end of input.
    fn run_select(&mut self, var: &str, items: &[String], body: &Program) -> Res {
        self.loops += 1;
        let mut status = 0;
        let result = (|| {
            let mut show_menu = true;
            loop {
                if show_menu {
                    let width = items.len().to_string().len();
                    let mut menu = String::new();
                    for (i, item) in items.iter().enumerate() {
                        menu.push_str(&format!("{:>width$}) {item}\n", i + 1));
                    }
                    let _ = sys::write_all(2, &text::encode(&menu));
                }
                let prompt = self.get("PS3").unwrap_or_else(|| "#? ".into());
                let _ = sys::write_all(2, &text::encode(&prompt));
                let Some(line) = builtins::read_line(0) else {
                    let _ = sys::write_all(2, b"\n");
                    return Ok(status);
                };
                let reply = line.trim().to_string();
                self.set_var("REPLY", &reply);
                if reply.is_empty() {
                    show_menu = true;
                    continue;
                }
                show_menu = false;
                let choice = reply.parse::<usize>().ok().and_then(|n| items.get(n.wrapping_sub(1)));
                let _ = self.assign(var, choice.cloned().unwrap_or_default(), false);
                match self.run_program(body) {
                    Ok(s) => status = s,
                    Err(Flow::Break(n)) => {
                        if n > 1 {
                            return Err(Flow::Break(n - 1));
                        }
                        return Ok(self.status);
                    }
                    Err(Flow::Continue(n)) => {
                        if n > 1 {
                            return Err(Flow::Continue(n - 1));
                        }
                    }
                    Err(flow) => return Err(flow),
                }
            }
        })();
        self.loops -= 1;
        result
    }

    /// Ctrl-C at an interactive shell stops a loop the shell runs itself.
    pub fn check_interrupt(&mut self) -> Result<(), Flow> {
        if self.interactive
            && self.trap_command(libc::SIGINT as usize).is_none()
            && trap::take_one(libc::SIGINT)
        {
            return Err(Flow::Interrupt);
        }
        Ok(())
    }

    // Simple commands.

    fn expand_assigns(&mut self, assigns: &[Assign]) -> Result<Vec<Assigned>, Flow> {
        let mut out = Vec::with_capacity(assigns.len());
        for a in assigns {
            out.push(self.expand_assign(a)?);
        }
        Ok(out)
    }

    fn expand_assign(&mut self, a: &Assign) -> Result<Assigned, Flow> {
        Ok(match &a.value {
            AssignValue::Scalar(word) => {
                let index = match &a.index {
                    Some(index) => Some(self.expand_quoted(index)?),
                    None => None,
                };
                let value = self.expand_assignment(word)?;
                Assigned::Scalar { name: a.name.clone(), index, append: a.append, value }
            }
            AssignValue::Array(elements) => {
                let mut values = Vec::new();
                for (key, word) in elements {
                    let key = match key {
                        Some(key) => Some(self.expand_quoted(key)?),
                        None => None,
                    };
                    if key.is_some() {
                        values.push((key, self.expand_assignment(word)?));
                    } else {
                        for value in self.expand_words(std::slice::from_ref(word))? {
                            values.push((None, value));
                        }
                    }
                }
                Assigned::Array { name: a.name.clone(), append: a.append, elements: values }
            }
        })
    }

    /// Makes assignments in the shell.
    pub fn apply_assigns(&mut self, assigns: Vec<Assigned>) -> Result<(), String> {
        for a in assigns {
            match a {
                Assigned::Scalar { name, index: Some(index), append, value } => {
                    self.assign_element(&name, &index, value, append)?
                }
                Assigned::Scalar { name, index: None, append, value } => {
                    self.assign(&name, value, append)?
                }
                Assigned::Array { name, append, elements } => {
                    self.assign_array(&name, elements, append)?
                }
            }
        }
        Ok(())
    }

    fn trace_assigns(&self, assigns: &[Assigned]) -> Vec<String> {
        assigns
            .iter()
            .map(|a| match a {
                Assigned::Scalar { name, index, append, value } => format!(
                    "{name}{}{}{}",
                    index.as_ref().map(|i| format!("[{i}]")).unwrap_or_default(),
                    if *append { "+=" } else { "=" },
                    crate::expand::quote(value)
                ),
                Assigned::Array { name, append, elements } => format!(
                    "{name}{}({})",
                    if *append { "+=" } else { "=" },
                    elements
                        .iter()
                        .map(|(_, v)| crate::expand::quote(v))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            })
            .collect()
    }

    /// Writes `set -x` output: `PS4` expanded, then the command.
    pub fn trace(&mut self, line: &str) {
        let ps4 = self.get("PS4").unwrap_or_default();
        let saved = self.opts.xtrace;
        self.opts.xtrace = false;
        let prefix = match crate::parse::string_word(&ps4, Rc::from("PS4"), self.parse_options()) {
            Ok(word) => self.expand_quoted(&word).unwrap_or(ps4.clone()),
            Err(_) => ps4.clone(),
        };
        self.opts.xtrace = saved;
        // The first character repeats for each level of nesting.
        let mut prefix = prefix;
        if let Some(first) = prefix.chars().next() {
            for _ in 0..self.subshell.min(8) {
                prefix.insert(0, first);
            }
        }
        let text = format!("{prefix}{line}\n");
        let _ = sys::write_all(2, &text::encode(&text));
    }

    /// The fields of a declaration command (`declare`, `local`, `export`,
    /// `readonly`, `typeset`): words that look like assignments are
    /// expanded as assignments, without splitting.
    fn expand_declaration(&mut self, words: &[Word]) -> Result<Vec<DeclArg>, Flow> {
        let mut out = Vec::new();
        for word in words {
            if let [WordPart::ArrayAssign(assign)] = word.parts.as_slice() {
                match self.expand_assign(assign)? {
                    Assigned::Array { name, append, elements } => {
                        out.push(DeclArg::Array { name, append, elements })
                    }
                    Assigned::Scalar { name, index, append, value } => {
                        out.push(DeclArg::Plain(format!(
                            "{name}{}{}{value}",
                            index.map(|i| format!("[{i}]")).unwrap_or_default(),
                            if append { "+=" } else { "=" }
                        )))
                    }
                }
                continue;
            }
            if let Some(WordPart::Lit(first)) = word.parts.first()
                && let Some(eq) = first.find('=')
                && crate::parse::is_name(
                    first[..eq].trim_end_matches('+').split('[').next().unwrap_or(""),
                )
            {
                // `name=value`: the value is not split or globbed.
                let mut rest = word.clone();
                let head = first[..=eq].to_string();
                let tail = first[eq + 1..].to_string();
                rest.parts[0] = WordPart::Lit(tail);
                if rest.parts.first() == Some(&WordPart::Lit(String::new())) {
                    rest.parts.remove(0);
                }
                let value = self.expand_assignment(&rest)?;
                out.push(DeclArg::Plain(format!("{head}{value}")));
                continue;
            }
            for field in self.expand_words(std::slice::from_ref(word))? {
                out.push(DeclArg::Plain(field));
            }
        }
        Ok(out)
    }

    pub fn run_simple(&mut self, command: &SimpleCommand, last: bool) -> Res {
        self.lineno = command.line;
        self.subst_status = None;
        self.check_interrupt()?;
        // The command name first: a declaration command expands the rest
        // its own way.
        let mut fields = Vec::new();
        let mut rest = 0;
        let mut decl = None;
        while rest < command.words.len() && fields.is_empty() {
            fields = self.expand_words(&command.words[rest..rest + 1])?;
            rest += 1;
        }
        if let Some(name) = fields.first()
            && builtins::is_declaration(name)
            && !self.functions.contains_key(name)
        {
            let mut args = vec![];
            for field in &fields[1..] {
                args.push(DeclArg::Plain(field.clone()));
            }
            args.extend(self.expand_declaration(&command.words[rest..])?);
            decl = Some(args);
        } else if rest < command.words.len() {
            fields.extend(self.expand_words(&command.words[rest..])?);
        }
        if fields.is_empty() {
            // Assignments alone, in the shell, each expanded after the one
            // before it is made (`IFS=, a=$(...)`); redirections alone,
            // undone. A failed assignment abandons the command line, and
            // ends a non-interactive shell.
            for assign in &command.assigns {
                let assigned = self.expand_assign(assign)?;
                if self.opts.xtrace {
                    let line = self.trace_assigns(std::slice::from_ref(&assigned)).join(" ");
                    self.trace(&line);
                }
                if let Err(message) = self.apply_assigns(vec![assigned]) {
                    self.error(&message);
                    return Err(Flow::Fatal(1));
                }
            }
            let saved = match self.redirect(&command.redirects, true) {
                Ok(saved) => saved,
                Err(Some(flow)) => return Err(flow),
                Err(None) => return Ok(1),
            };
            self.restore(saved);
            return Ok(self.subst_status.unwrap_or(0));
        }
        let assigns = self.expand_assigns(&command.assigns)?;

        if self.opts.xtrace {
            let mut parts = self.trace_assigns(&assigns);
            match &decl {
                Some(args) => {
                    parts.push(crate::expand::quote(&fields[0]));
                    parts.extend(args.iter().map(|a| match a {
                        DeclArg::Plain(s) => crate::expand::quote(s),
                        DeclArg::Array { name, .. } => format!("{name}=(...)"),
                    }));
                }
                None => parts.extend(fields.iter().map(|f| crate::expand::quote(f))),
            }
            if !parts.is_empty() {
                self.trace(&parts.join(" "));
            }
        }

        let name = fields[0].clone();
        if self.opts.restricted && name.contains('/') {
            self.error(&format!("{name}: restricted: cannot specify `/' in command names"));
            return Ok(1);
        }

        // `exec` without a command makes its redirections stay.
        if name == "exec" && fields.len() == 1 && !self.functions.contains_key("exec") {
            if let Err(message) = self.apply_assigns(assigns) {
                self.error(&message);
                return Ok(1);
            }
            return match self.redirect(&command.redirects, false) {
                Ok(_) => Ok(0),
                Err(Some(flow)) => Err(flow),
                Err(None) => {
                    if !self.interactive {
                        return Err(Flow::Exit(1));
                    }
                    Ok(1)
                }
            };
        }

        if let Some(args) = decl {
            let builtin = builtins::lookup(&name).ok_or(Flow::Fatal(1))?;
            return self.run_builtin(
                builtin,
                &name,
                BuiltinArgs::Decl(args),
                assigns,
                &command.redirects,
            );
        }

        let special = builtins::is_special(&name);
        if special
            && (self.posix() || !self.functions.contains_key(&name))
            && let Some(builtin) = builtins::lookup(&name)
        {
            return self.run_builtin(
                builtin,
                &name,
                BuiltinArgs::Plain(fields),
                assigns,
                &command.redirects,
            );
        }
        if let Some(def) = self.functions.get(&name).cloned() {
            return self.run_function_with(def, fields, assigns, &command.redirects);
        }
        if let Some(builtin) = builtins::lookup(&name)
            && builtins::enabled(self, &name)
        {
            return self.run_builtin(
                builtin,
                &name,
                BuiltinArgs::Plain(fields),
                assigns,
                &command.redirects,
            );
        }
        self.run_external(fields, assigns, &command.redirects, last)
    }

    fn run_builtin(
        &mut self,
        builtin: builtins::Builtin,
        name: &str,
        args: BuiltinArgs,
        assigns: Vec<Assigned>,
        redirects: &[Redirect],
    ) -> Res {
        let special = builtins::is_special(name);
        // Assignments before a special builtin stay in POSIX mode; before
        // anything else they last for the command.
        let temp = !assigns.is_empty() && !(special && self.posix());
        if temp {
            self.vars.push_temp();
            for a in assigns {
                if let Assigned::Scalar { name, index: None, value, append } = a {
                    let value =
                        if append { self.get(&name).unwrap_or_default() + &value } else { value };
                    let exported = self.vars.get(&name).is_some_and(|v| v.has(attr::EXPORT));
                    self.vars.set_temp(&name, temp_var(value, exported));
                }
            }
        } else if let Err(message) = self.apply_assigns(assigns) {
            self.error(&message);
            return if !self.interactive { Err(Flow::Exit(1)) } else { Ok(1) };
        }
        let saved = match self.redirect(redirects, true) {
            Ok(saved) => saved,
            Err(error) => {
                if temp {
                    self.vars.pop();
                }
                if let Some(flow) = error {
                    return Err(flow);
                }
                if special && self.posix() && !self.interactive {
                    return Err(Flow::Exit(1));
                }
                return Ok(1);
            }
        };
        let result = match args {
            BuiltinArgs::Plain(fields) => (builtin.run)(self, &fields),
            BuiltinArgs::Decl(args) => builtins::run_declaration(self, name, args),
        };
        self.restore(saved);
        if temp {
            self.vars.pop();
        }
        // A special builtin's error ends a non-interactive POSIX shell.
        match result {
            Err(Flow::Fatal(status)) if special && self.posix() && !self.interactive => {
                Err(Flow::Exit(status))
            }
            other => other,
        }
    }

    fn run_function_with(
        &mut self,
        def: Rc<FunctionDef>,
        fields: Vec<String>,
        assigns: Vec<Assigned>,
        redirects: &[Redirect],
    ) -> Res {
        let temp = !assigns.is_empty();
        if temp {
            self.vars.push_temp();
            for a in assigns {
                if let Assigned::Scalar { name, index: None, value, .. } = a {
                    self.vars.set_temp(&name, temp_var(value, true));
                }
            }
        }
        let saved = match self.redirect(redirects, true) {
            Ok(saved) => saved,
            Err(error) => {
                if temp {
                    self.vars.pop();
                }
                return match error {
                    Some(flow) => Err(flow),
                    None => Ok(1),
                };
            }
        };
        let result = self.call_function(def, &fields);
        self.restore(saved);
        if temp {
            self.vars.pop();
        }
        result
    }

    /// Calls a function with `fields` (its name, then its arguments).
    pub fn call_function(&mut self, def: Rc<FunctionDef>, fields: &[String]) -> Res {
        let nest = self.frames.iter().filter(|f| f.name != "source").count();
        let limit = self.get("FUNCNEST").and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0);
        if nest >= limit.unwrap_or(MAX_NESTING) {
            return Err(self.fatal(
                &format!(
                    "{}: maximum function nesting level exceeded ({})",
                    def.name,
                    limit.unwrap_or(MAX_NESTING)
                ),
                1,
            ));
        }
        let args = std::mem::replace(&mut self.args, fields[1..].to_vec());
        let loops = std::mem::replace(&mut self.loops, 0);
        self.frames.push(Frame {
            name: def.name.clone(),
            file: def.file.clone(),
            call_line: self.lineno,
            saved_opts: None,
        });
        self.vars.push_function();
        let saved_return = if self.opts.functrace { None } else { self.traps[trap::RETURN].take() };
        let result = self.run_command(&def.body, false);
        let result = match result {
            Err(Flow::Return(status)) => Ok(status),
            other => other,
        };
        if self.trap_command(trap::RETURN).is_some() {
            let status = self.flow_status(result);
            let _ = self.run_trap(trap::RETURN);
            self.status = status;
        }
        if let Some(saved) = saved_return {
            self.traps[trap::RETURN] = Some(saved);
        }
        self.vars.pop();
        if let Some(opts) = self.frames.pop().and_then(|frame| frame.saved_opts) {
            // `local -`: the options as they were.
            let monitor = self.opts.monitor;
            self.opts = *opts;
            if self.opts.monitor != monitor {
                self.monitor_changed();
            }
        }
        self.loops = loops;
        self.args = args;
        result
    }

    // Programs.

    /// Where `name` is on `PATH`, if anywhere.
    pub fn find_command(&mut self, name: &str) -> Option<String> {
        if let Some(path) = self.hash.get(name) {
            if sys::executable(path) {
                let path = path.clone();
                *self.hash_hits.entry(name.to_string()).or_insert(0) += 1;
                return Some(path);
            }
            self.hash.remove(name);
        }
        let path = self.get("PATH").unwrap_or_default();
        for dir in path.split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let candidate = format!("{}/{name}", dir.trim_end_matches('/'));
            if let Ok(meta) = std::fs::metadata(text::to_os(&candidate))
                && meta.is_file()
                && sys::executable(&candidate)
            {
                if self.opts.hashall {
                    self.hash.insert(name.to_string(), candidate.clone());
                    self.hash_hits.insert(name.to_string(), 1);
                }
                return Some(candidate);
            }
        }
        None
    }

    /// `command name args`: the program on `PATH`, never a function.
    pub fn run_program_fields(&mut self, fields: Vec<String>) -> Res {
        self.run_external(fields, Vec::new(), &[], false)
    }

    fn run_external(
        &mut self,
        fields: Vec<String>,
        assigns: Vec<Assigned>,
        redirects: &[Redirect],
        last: bool,
    ) -> Res {
        let name = &fields[0];
        let path = if name.contains('/') { Some(name.clone()) } else { self.find_command(name) };
        let Some(path) = path else {
            if let Some(handler) = self.functions.get("command_not_found_handle").cloned() {
                let mut handler_args = vec!["command_not_found_handle".to_string()];
                handler_args.extend(fields.iter().cloned());
                let pid = self.fork_child(0, false)?;
                if pid == 0 {
                    let result = self.call_function(handler, &handler_args);
                    self.exit_child(result);
                }
                return Ok(self.wait_child(pid));
            }
            // The redirections still happen, as in other shells.
            if let Ok(saved) = self.redirect(redirects, true) {
                self.error(&format!("{name}: command not found"));
                self.restore(saved);
            } else {
                self.error(&format!("{name}: command not found"));
            }
            return Ok(127);
        };
        if last && self.can_exec_directly() {
            self.exec_program(&path, &fields, assigns, redirects);
        }
        let pid = self.fork_child(0, false)?;
        if pid == 0 {
            self.exec_program(&path, &fields, assigns, redirects);
        }
        let id = self.add_job(pid, &[pid], fields.join(" "), false);
        self.wait_job(id)
    }

    /// Whether the shell can let the program it runs last replace it: in
    /// a child, with no EXIT trap left to run.
    fn can_exec_directly(&self) -> bool {
        self.subshell > 0 && self.trap_command(trap::EXIT).is_none() && !self.has_procsubs()
    }

    /// In a child: redirects, sets the environment and runs the program,
    /// never returning.
    pub fn exec_program(
        &mut self,
        path: &str,
        fields: &[String],
        assigns: Vec<Assigned>,
        redirects: &[Redirect],
    ) -> ! {
        if self.subshell == 0 {
            self.become_subshell();
        }
        if self.redirect(redirects, false).is_err() {
            sys::exit_now(1);
        }
        for a in assigns {
            if let Assigned::Scalar { name, index: None, value, append } = a {
                let value =
                    if append { self.get(&name).unwrap_or_default() + &value } else { value };
                let var = self.vars.entry(&name);
                var.value = Some(crate::vars::Value::Str(value));
                var.attrs |= attr::EXPORT;
            }
        }
        self.reset_signals_for_exec();
        let argv: Vec<_> = fields.iter().map(|f| text::cstring(f)).collect();
        let envp: Vec<_> = self.environment().iter().map(|e| text::cstring(e)).collect();
        let c_path = text::cstring(path);
        let error = sys::execve(&c_path, &argv, &envp);
        match error.raw_os_error() {
            Some(libc::ENOEXEC) => {
                // A text file without `#!`: the shell runs it.
                let status = self.run_script_file(path, &fields[1..]);
                sys::exit_now(status);
            }
            Some(libc::ENOENT) if !path.contains('/') || fields[0].contains('/') => {
                self.error(&format!("{}: No such file or directory", fields[0]));
                sys::exit_now(127);
            }
            Some(libc::ENOENT) => {
                self.error(&format!("{}: command not found", fields[0]));
                sys::exit_now(127);
            }
            _ => {
                let what = if std::fs::metadata(text::to_os(path)).is_ok_and(|m| m.is_dir()) {
                    "Is a directory".to_string()
                } else {
                    sys::describe(&error)
                };
                self.error(&format!("{}: {what}", fields[0]));
                sys::exit_now(126);
            }
        }
    }

    /// Puts signals the shell catches back to their default, for a
    /// program about to run; ignored ones stay ignored.
    pub fn reset_signals_for_exec(&self) {
        for (_, signal) in trap::SIGNALS {
            let slot = *signal as usize;
            let ignored = self.traps.get(slot).is_some_and(|t| t.as_deref() == Some(""))
                || self.ignored[slot];
            if !ignored && !matches!(*signal, libc::SIGKILL | libc::SIGSTOP) {
                trap::set(*signal, trap::Disposition::Default);
            }
        }
        trap::unblock_all();
    }

    /// Runs a text file as a script in this process (a child), as for a
    /// program without `#!`.
    pub fn run_script_file(&mut self, path: &str, args: &[String]) -> i32 {
        let bytes = match std::fs::read(text::to_os(path)) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.error(&format!("{path}: {}", error));
                return 126;
            }
        };
        if bytes.iter().take(1024).any(|&b| b == 0) {
            self.error(&format!("{path}: cannot execute binary file"));
            return 126;
        }
        self.functions.clear();
        self.aliases.clear();
        self.args = args.to_vec();
        self.arg0 = path.to_string();
        self.source = Rc::from(path);
        let result = self.run_text(&text::decode(&bytes), Rc::from(path));
        let status = self.flow_status(result);
        self.run_exit_trap(status)
    }

    /// Waits for a subshell's process.
    pub fn wait_child(&mut self, pid: i32) -> i32 {
        let id = self.add_job(pid, &[pid], String::from("subshell"), false);
        self.wait_job(id).unwrap_or(1)
    }

    // Substitutions.

    /// `$(...)`: the program's output, its final newlines taken off.
    pub fn command_subst(&mut self, program: &Program) -> Result<String, Flow> {
        // `$(< file)` reads the file without running anything.
        if let [complete] = program.commands.as_slice()
            && let [(AndOr { first, rest }, Sep::Seq)] = complete.items.as_slice()
            && rest.is_empty()
            && first.commands.len() == 1
            && !first.bang
            && let Command::Simple(simple) = &first.commands[0]
            && simple.words.is_empty()
            && simple.assigns.is_empty()
            && let [Redirect { fd: None, op: RedirOp::Read, target: RedirTarget::Word(word) }] =
                simple.redirects.as_slice()
        {
            let path = self.expand_string(word)?;
            return match std::fs::read(text::to_os(&path)) {
                Ok(bytes) => {
                    self.subst_status = Some(0);
                    Ok(strip_newlines(text::decode(&bytes)))
                }
                Err(error) => {
                    self.error(&format!("{path}: {}", sys::describe(&error)));
                    self.subst_status = Some(1);
                    Ok(String::new())
                }
            };
        }
        let (read, write) = match sys::pipe() {
            Ok(fds) => fds,
            Err(error) => {
                return Err(self.fatal(
                    &format!(
                        "cannot make a pipe for command substitution: {}",
                        sys::describe(&error)
                    ),
                    1,
                ));
            }
        };
        let monitor = std::mem::replace(&mut self.opts.monitor, false);
        let pid = self.fork_child(0, true);
        self.opts.monitor = monitor;
        let pid = pid?;
        if pid == 0 {
            sys::close(read);
            let _ = sys::dup2(write, 1);
            sys::close(write);
            if !self.posix() && !self.shopt("inherit_errexit") {
                self.opts.errexit = false;
            }
            let single = matches!(program.commands.as_slice(), [c] if c.items.len() == 1);
            let result = if single
                && let [complete] = program.commands.as_slice()
                && let [(list, Sep::Seq)] = complete.items.as_slice()
                && list.rest.is_empty()
                && list.first.commands.len() == 1
                && !list.first.bang
                && list.first.time.is_none()
            {
                self.run_command(&list.first.commands[0], true)
            } else {
                self.run_program(program)
            };
            self.exit_child(result);
        }
        sys::close(write);
        let output = sys::read_all(read).unwrap_or_default();
        sys::close(read);
        let status = self.wait_pid(pid);
        self.subst_status = Some(status);
        self.status = status;
        let mut output = output;
        if output.contains(&0) {
            if !self.posix() {
                self.error("warning: command substitution: ignored null byte in input");
            }
            output.retain(|&b| b != 0);
        }
        Ok(strip_newlines(text::decode(&output)))
    }

    /// `<(...)` and `>(...)`: a child reading or writing a pipe, and the
    /// pipe's name in `/dev/fd`.
    pub fn process_subst(&mut self, output: bool, program: &Program) -> Result<String, Flow> {
        let (read, write) = match sys::pipe() {
            Ok(fds) => fds,
            Err(error) => {
                return Err(self.fatal(
                    &format!(
                        "cannot make a pipe for process substitution: {}",
                        sys::describe(&error)
                    ),
                    1,
                ));
            }
        };
        let monitor = std::mem::replace(&mut self.opts.monitor, false);
        let pid = self.fork_child(0, true);
        self.opts.monitor = monitor;
        let pid = pid?;
        if pid == 0 {
            if output {
                sys::close(write);
                let _ = sys::dup2(read, 0);
                sys::close(read);
            } else {
                sys::close(read);
                let _ = sys::dup2(write, 1);
                sys::close(write);
            }
            let result = self.run_program(program);
            self.exit_child(result);
        }
        self.last_bg = Some(pid);
        let (keep, other) = if output { (write, read) } else { (read, write) };
        sys::close(other);
        // The command gets the descriptor; the shell closes it afterwards.
        // SAFETY: F_DUPFD only acts on the descriptor table.
        let fd = unsafe { libc::fcntl(keep, libc::F_DUPFD, 63) };
        sys::close(keep);
        if fd < 0 {
            return Err(self.fatal("cannot make a descriptor for process substitution", 1));
        }
        self.procsub_fds.push(fd);
        Ok(format!("/dev/fd/{fd}"))
    }

    fn has_procsubs(&self) -> bool {
        !self.procsub_fds.is_empty()
    }

    /// Closes the descriptors of finished process substitutions.
    pub fn close_procsubs(&mut self) {
        for fd in self.procsub_fds.drain(..) {
            sys::close(fd);
        }
    }

    // Traps.

    /// Runs the traps of signals that came.
    pub fn run_pending_traps(&mut self) -> Result<(), Flow> {
        if !trap::pending() || self.in_trap {
            return Ok(());
        }
        for signal in trap::take() {
            if signal == libc::SIGCHLD && self.trap_command(signal as usize).is_none() {
                continue;
            }
            if signal == libc::SIGINT
                && self.interactive
                && self.trap_command(signal as usize).is_none()
            {
                return Err(Flow::Interrupt);
            }
            if signal == libc::SIGHUP
                && self.interactive
                && self.trap_command(signal as usize).is_none()
            {
                self.hang_up_jobs();
                return Err(Flow::Exit(128 + libc::SIGHUP));
            }
            if signal == libc::SIGWINCH {
                self.update_window_size();
            }
            if self.trap_command(signal as usize).is_some() {
                self.run_trap(signal as usize)?;
            }
        }
        Ok(())
    }

    /// Runs one trap's command; `$?` is kept across it.
    pub fn run_trap(&mut self, slot: usize) -> Result<(), Flow> {
        let Some(command) = self.trap_command(slot).map(str::to_string) else { return Ok(()) };
        let status = self.status;
        let was = std::mem::replace(&mut self.in_trap, true);
        self.errexit_off += 1;
        let result = self.eval_text(&command, "trap");
        self.errexit_off -= 1;
        self.in_trap = was;
        self.status = status;
        match result {
            Err(Flow::Exit(code)) => Err(Flow::Exit(code)),
            Err(Flow::Return(code)) if slot != trap::RETURN => Err(Flow::Return(code)),
            _ => Ok(()),
        }
    }

    pub fn update_window_size(&mut self) {
        if let Some((cols, rows)) = sys::window_size(2).or_else(|| sys::window_size(0)) {
            self.set_var("COLUMNS", &cols.to_string());
            self.set_var("LINES", &rows.to_string());
        }
    }

    // Reading and running text.

    /// Parses and runs `text` (from `eval`, a trap, `-c`): one command at
    /// a time, so that aliases defined early apply later.
    pub fn eval_text(&mut self, source_text: &str, name: &str) -> Res {
        let file = Rc::from(name);
        let saved = self.source.clone();
        let result = self.run_text_from(source_text, file, self.lineno.max(1));
        self.source = saved;
        result
    }

    /// Runs a script's text.
    pub fn run_text(&mut self, source_text: &str, file: Rc<str>) -> Res {
        self.run_text_from(source_text, file, 1)
    }

    fn run_text_from(&mut self, source_text: &str, file: Rc<str>, line: u32) -> Res {
        let mut parser = crate::parse::Parser::new(source_text, file, line, self.parse_options());
        let mut status = 0;
        loop {
            parser.set_options(self.parse_options());
            match parser.next_command() {
                Ok(Some(command)) => {
                    if self.opts.noexec && !self.interactive {
                        continue;
                    }
                    status = self.run_complete(&command)?;
                    self.close_procsubs();
                    self.notify_background();
                }
                Ok(None) => return Ok(status),
                Err(error) => {
                    let line = error.line().unwrap_or(parser.line());
                    self.lineno = line;
                    let message = match &error {
                        crate::parse::Error::Incomplete(message) => {
                            format!("syntax error: unexpected end of file ({message})")
                        }
                        crate::parse::Error::Syntax { message, .. } => {
                            format!("syntax error: {message}")
                        }
                    };
                    self.error(&message);
                    self.syntax_failed = true;
                    return Err(Flow::Fatal(2));
                }
            }
        }
    }

    /// Without job control, finished background jobs are noted quietly.
    pub fn notify_background(&mut self) {
        if !self.interactive && !self.jobs.list.is_empty() {
            self.notify_jobs();
        }
    }
}

/// The arguments of a builtin.
pub enum BuiltinArgs {
    Plain(Vec<String>),
    Decl(Vec<DeclArg>),
}

/// Takes the final newlines off a command substitution's output.
fn strip_newlines(mut text: String) -> String {
    while text.ends_with('\n') {
        text.pop();
    }
    text
}
