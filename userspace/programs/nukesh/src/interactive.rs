// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The interactive shell: job control on the terminal, and the loop that
//! shows the prompt, reads a command, puts it in the history and runs it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::editor::{Editor, Input};
use crate::history::Expanded;
use crate::parse::Parser;
use crate::shell::{Flow, Shell};
use crate::vars::Value;
use crate::{sys, text, trap};

/// The process group that had the terminal before the shell took it,
/// which gets it back when the shell exits.
static ORIGINAL_PGRP: AtomicI32 = AtomicI32::new(0);

/// Takes the terminal for job control: waits until the shell is in the
/// foreground, puts it in a process group of its own and gives that group
/// the terminal. Without a terminal, job control stays off.
pub fn take_terminal_control(shell: &mut Shell) {
    let Some(fd) = [2, 0, 1].into_iter().find(|fd| sys::isatty(*fd)) else {
        no_job_control(shell, "no terminal");
        return;
    };
    let Ok(tty) = sys::dup_high(fd) else {
        no_job_control(shell, "cannot keep the terminal open");
        return;
    };
    // A shell started in the background stops until it is brought to the
    // foreground, as other shells do.
    loop {
        let Ok(foreground) = sys::tcgetpgrp(tty) else {
            sys::close(tty);
            no_job_control(shell, "cannot get the terminal's process group");
            return;
        };
        let own = sys::getpgrp();
        if foreground == own {
            break;
        }
        trap::set(libc::SIGTTIN, trap::Disposition::Default);
        let _ = sys::kill(-own, libc::SIGTTIN);
    }
    ORIGINAL_PGRP.store(sys::getpgrp(), Ordering::Relaxed);
    let pid = sys::getpid();
    // tcsetpgrp from a group not in the foreground raises SIGTTOU.
    trap::set(libc::SIGTTOU, trap::Disposition::Ignore);
    if sys::getpgrp() != pid && sys::setpgid(0, pid).is_err() {
        sys::close(tty);
        no_job_control(shell, "cannot set the process group");
        return;
    }
    if sys::tcsetpgrp(tty, pid).is_err() {
        sys::close(tty);
        no_job_control(shell, "cannot set the terminal's process group");
        return;
    }
    shell.tty = Some(tty);
    shell.shell_pgid = pid;
    shell.tty_modes = sys::tcgetattr(tty);
}

/// `set -m` in a script: when the script runs in the foreground of the
/// terminal on its standard error, the jobs it runs in the foreground
/// have the terminal while they run, as in bash. The shell stays in its
/// own process group, and the terminal's modes are the jobs' to change.
pub fn share_terminal(shell: &mut Shell) {
    if !sys::isatty(2) || sys::tcgetpgrp(2).ok() != Some(sys::getpgrp()) {
        return;
    }
    if let Ok(tty) = sys::dup_high(2) {
        shell.tty = Some(tty);
        shell.shell_pgid = sys::getpgrp();
        shell.tty_modes = None;
    }
}

fn no_job_control(shell: &mut Shell, why: &str) {
    shell.error(&format!("{why}: no job control in this shell"));
    shell.opts.monitor = false;
}

/// Gives the terminal back to the process group that had it.
fn release_terminal(shell: &mut Shell) {
    let original = ORIGINAL_PGRP.load(Ordering::Relaxed);
    if let Some(tty) = shell.tty
        && original > 0
        && original != shell.shell_pgid
    {
        let _ = sys::setpgid(0, original);
        let _ = sys::tcsetpgrp(tty, original);
    }
}

/// How many times Ctrl-D in a row `set -o ignoreeof` refuses: `IGNOREEOF`,
/// or 10.
fn eof_limit(shell: &Shell) -> u32 {
    match shell.get("IGNOREEOF") {
        Some(value) => value.trim().parse().unwrap_or(10),
        None => 10,
    }
}

/// Runs the interactive shell until it exits; the status it exits with.
pub fn run(shell: Shell) -> i32 {
    let shell = Rc::new(RefCell::new(shell));
    shell.borrow_mut().load_history();
    let mut editor = Editor::new(shell.clone());
    let mut eofs = 0;
    let mut line_number = 1;
    loop {
        if let Some(code) = before_prompt(&mut shell.borrow_mut()) {
            return exit(&mut shell.borrow_mut(), code);
        }
        let input = editor.read_command();
        let mut shell = shell.borrow_mut();
        match input {
            Input::Command(text) => {
                eofs = 0;
                let lines = text.matches('\n').count() as u32 + 1;
                let result = run_line(&mut shell, &text, line_number);
                line_number += lines;
                if let Some(code) = result {
                    if code == EXIT_REFUSED {
                        continue;
                    }
                    return exit(&mut shell, code);
                }
            }
            Input::Interrupted => {
                shell.status = 130;
                // The SIGINT trap, if one is set.
                if let Err(Flow::Exit(code)) = shell.run_pending_traps() {
                    return exit(&mut shell, code);
                }
            }
            Input::Signal => {}
            Input::TimedOut => {
                let _ = sys::write_all(2, b"\ntimed out waiting for input: auto-logout\n");
                let status = shell.status;
                return exit(&mut shell, status);
            }
            Input::Eof => {
                if shell.opts.ignoreeof && eofs < eof_limit(&shell) {
                    eofs += 1;
                    let word = if shell.login { "logout" } else { "exit" };
                    let _ = sys::write_all(
                        2,
                        text::encode(&format!("Use \"{word}\" to leave the shell.\n")).as_slice(),
                    );
                    continue;
                }
                if refuse_exit(&mut shell) {
                    continue;
                }
                let word = if shell.login { "logout\n" } else { "exit\n" };
                let _ = sys::write_all(2, word.as_bytes());
                let status = shell.status;
                return exit(&mut shell, status);
            }
        }
    }
}

/// What [`run_line`] gives back when `exit` was refused for stopped jobs.
const EXIT_REFUSED: i32 = -1;

/// Ctrl-D with stopped jobs: refused once, as `exit` is.
fn refuse_exit(shell: &mut Shell) -> bool {
    shell.reap();
    if shell.jobs.list.iter().any(|job| job.stopped()) && !shell.exit_warned {
        shell.exit_warned = true;
        let _ = sys::write_all(2, b"There are stopped jobs.\n");
        return true;
    }
    false
}

/// What comes before each prompt: background jobs that changed, traps of
/// signals that came, the window size, and `PROMPT_COMMAND`. An exit
/// asked for on the way is its status.
fn before_prompt(shell: &mut Shell) -> Option<i32> {
    shell.notify_jobs();
    shell.reap_coprocs();
    if let Err(Flow::Exit(code)) = shell.run_pending_traps() {
        return Some(code);
    }
    if shell.shopt("checkwinsize") {
        shell.update_window_size();
    }
    let commands: Vec<String> = match shell.get_value("PROMPT_COMMAND") {
        Some(Value::Array(elements)) => elements.into_values().collect(),
        Some(Value::Str(command)) => vec![command],
        _ => Vec::new(),
    };
    for command in commands.iter().filter(|c| !c.is_empty()) {
        let status = shell.status;
        let result = shell.eval_text(command, "PROMPT_COMMAND", "PROMPT_COMMAND");
        shell.status = status;
        if let Err(Flow::Exit(code)) = result {
            return Some(code);
        }
    }
    None
}

/// Runs one command read at the prompt, after `!` expansion and putting
/// it in the history; an exit is its status.
fn run_line(shell: &mut Shell, line: &str, first_line: u32) -> Option<i32> {
    let line = if shell.history.expand && line.contains(['!', '^']) {
        match shell.expand_history(line) {
            Ok(Expanded::Same) => line.to_string(),
            Ok(Expanded::Run(expanded)) => {
                let _ = sys::write_all(2, text::encode(&format!("{expanded}\n")).as_slice());
                expanded
            }
            Ok(Expanded::Print(expanded)) => {
                shell.add_history(&expanded);
                let _ = sys::write_all(1, text::encode(&format!("{expanded}\n")).as_slice());
                return None;
            }
            Err(message) => {
                shell.error(&message);
                shell.status = 1;
                return None;
            }
        }
    } else {
        line.to_string()
    };
    if line.trim().is_empty() {
        return None;
    }
    shell.add_history(&line);
    if let Some(ps0) = shell.get("PS0").filter(|p| !p.is_empty()) {
        let text = crate::prompt::expand(shell, &ps0);
        let _ = sys::write_all(2, &text::encode(&text));
    }
    let mut parser =
        Parser::new(&line, Rc::from(shell.name.as_str()), first_line, shell.parse_options());
    loop {
        parser.set_options(shell.parse_options());
        let next = parser.next_command();
        shell.parse_warnings(&mut parser);
        match next {
            Ok(Some(command)) => {
                let result = shell.run_complete(&command);
                shell.close_procsubs();
                shell.command_number += 1;
                match result {
                    Ok(status) => shell.status = status,
                    Err(Flow::Exit(code)) => {
                        if refuse_exit(shell) {
                            return Some(EXIT_REFUSED);
                        }
                        return Some(code);
                    }
                    // The rest of the line is abandoned.
                    Err(Flow::Fatal(status) | Flow::Discard(status)) => {
                        shell.status = status;
                        return None;
                    }
                    Err(Flow::Interrupt) => {
                        shell.status = 130;
                        let _ = sys::write_all(2, b"\n");
                        return None;
                    }
                    Err(Flow::Return(status)) => shell.status = status,
                    Err(Flow::Break(_) | Flow::Continue(_)) => {}
                }
            }
            Ok(None) => return None,
            Err(error) => {
                shell.syntax_error(&error, None, parser.end_line());
                shell.status = 2;
                return None;
            }
        }
    }
}

/// Leaves the shell: the EXIT trap, a login shell's logout file, the
/// history file, hanging up the jobs with `huponexit`, and the terminal
/// given back.
fn exit(shell: &mut Shell, code: i32) -> i32 {
    let code = shell.run_exit_trap(code);
    if shell.login
        && let Some(home) = shell.get("HOME")
    {
        let logout = format!("{home}/.nukesh_logout");
        if std::fs::metadata(text::to_os(&logout)).is_ok() {
            let _ = crate::builtins::source_file(shell, &logout);
        }
    }
    shell.save_history();
    if shell.login && shell.shopt("huponexit") {
        shell.hang_up_jobs();
    }
    release_terminal(shell);
    code
}
