// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The builtins: commands the shell runs itself.
//!
//! POSIX's special builtins (XCU 2.14) come before functions, and an error
//! in one ends a non-interactive POSIX shell; the declaration builtins
//! (`declare`, `local`, `export`, `readonly`, `typeset`) have their
//! assignment arguments expanded as assignments, without splitting. The
//! rest are looked up after functions, before `PATH`.

mod declare;
mod dirs;
mod io;
mod job;
mod misc;

use std::rc::Rc;

pub use declare::{DeclArg, declare_line, double_quote, quote_key, run_declaration, set_option};
pub use io::{Escapes, escapes, read_line};
pub use misc::KEYWORDS;

use crate::shell::{Flow, Frame, Res, Shell};
use crate::{sys, text, trap};

pub type Run = fn(&mut Shell, &[String]) -> Res;

#[derive(Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    pub run: Run,
    /// The usage line, for errors and `help`.
    pub usage: &'static str,
    /// What it does, in a line, for `help`.
    pub summary: &'static str,
}

macro_rules! table {
    ($($name:literal => $run:path, $usage:literal, $summary:literal;)*) => {
        pub static TABLE: &[Builtin] = &[
            $(Builtin { name: $name, run: $run, usage: $usage, summary: $summary },)*
        ];
    };
}

table! {
    "." => source, ". filename [arguments]", "Run the commands of a file in this shell.";
    ":" => colon, ": [arguments]", "Do nothing, successfully.";
    "[" => crate::cond::test_builtin, "[ expression ]", "Evaluate a conditional expression.";
    "alias" => misc::alias, "alias [-p] [name[=value] ...]", "Define or show aliases.";
    "bg" => job::bg, "bg [job ...]", "Continue stopped jobs in the background.";
    "break" => break_, "break [n]", "Leave n enclosing loops.";
    "builtin" => builtin, "builtin [builtin [arguments]]", "Run a builtin, skipping functions.";
    "caller" => misc::caller, "caller [n]", "Show where the current function was called from.";
    "cd" => dirs::cd, "cd [-L|-P [-e]] [dir]", "Change the working directory.";
    "command" => command, "command [-pVv] command [arguments]", "Run a command, skipping functions, or describe it.";
    "compgen" => crate::complete::compgen, "compgen [-abcdefgjksuv] [-o option] [-A action] [-G glob] [-W words] [-F function] [-C command] [-X filter] [-P prefix] [-S suffix] [word]", "Show the completions of a word.";
    "complete" => crate::complete::complete, "complete [-abcdefgjksuv] [-pr] [-DEI] [-o option] [-A action] [-G glob] [-W words] [-F function] [-C command] [-X filter] [-P prefix] [-S suffix] [name ...]", "Set how a command's arguments are completed.";
    "compopt" => crate::complete::compopt, "compopt [-o|+o option] [-DEI] [name ...]", "Change completion options.";
    "continue" => continue_, "continue [n]", "Go on with the next pass of the nth enclosing loop.";
    "declare" => declare::declare, "declare [-aAfFgiIlnrtux] [-p] [name[=value] ...]", "Set variables' values and attributes.";
    "dirs" => dirs::dirs, "dirs [-clpv] [+N] [-N]", "Show the directory stack.";
    "disown" => job::disown, "disown [-h] [-ar] [job ...]", "Forget jobs, or keep them from a hang-up.";
    "echo" => io::echo, "echo [-neE] [argument ...]", "Write the arguments.";
    "enable" => misc::enable, "enable [-a] [-nps] [name ...]", "Turn builtins on and off.";
    "eval" => eval, "eval [argument ...]", "Run the arguments as shell commands.";
    "exec" => exec, "exec [-cl] [-a name] [command [argument ...]]", "Replace the shell with a program.";
    "exit" => exit, "exit [n]", "Leave the shell.";
    "export" => declare::export, "export [-fn] [-p] [name[=value] ...]", "Put variables in the environment of programs.";
    "false" => false_, "false", "Fail.";
    "fc" => crate::history::fc, "fc [-e ename] [-lnr] [first] [last] or fc -s [pat=rep] [command]", "List, edit and run commands from the history.";
    "fg" => job::fg, "fg [job]", "Bring a job to the foreground.";
    "getopts" => misc::getopts, "getopts optstring name [argument ...]", "Parse options.";
    "hash" => misc::hash, "hash [-lr] [-p path] [-dt] [name ...]", "Remember or show where programs are.";
    "help" => misc::help, "help [-ds] [pattern ...]", "Describe the builtins.";
    "history" => crate::history::history, "history [-c] [-d offset] [n] or history -anrw [file] or history -ps argument ...", "Show or change the command history.";
    "jobs" => job::jobs, "jobs [-lnprs] [job ...]", "List the jobs.";
    "kill" => job::kill, "kill [-s signal | -n number | -signal] pid | job ... or kill -l [signal]", "Send a signal.";
    "let" => let_, "let expression [expression ...]", "Evaluate arithmetic.";
    "local" => declare::local, "local [option] [name[=value] ...]", "Make variables local to a function.";
    "logout" => logout, "logout [n]", "Leave a login shell.";
    "mapfile" => io::mapfile, "mapfile [-d delim] [-n count] [-O origin] [-s count] [-t] [-u fd] [-C callback [-c quantum]] [array]", "Read lines into an array.";
    "popd" => dirs::popd, "popd [-n] [+N | -N]", "Take a directory off the stack, and go there.";
    "printf" => io::printf, "printf [-v var] format [arguments]", "Write formatted output.";
    "pushd" => dirs::pushd, "pushd [-n] [+N | -N | dir]", "Put a directory on the stack, and go there.";
    "pwd" => dirs::pwd, "pwd [-LP]", "Show the working directory.";
    "read" => io::read, "read [-ers] [-a array] [-d delim] [-i text] [-n nchars] [-N nchars] [-p prompt] [-t timeout] [-u fd] [name ...]", "Read a line into variables.";
    "readarray" => io::mapfile, "readarray [-d delim] [-n count] [-O origin] [-s count] [-t] [-u fd] [-C callback [-c quantum]] [array]", "Read lines into an array.";
    "readonly" => declare::readonly, "readonly [-aAf] [-p] [name[=value] ...]", "Make variables unchangeable.";
    "return" => return_, "return [n]", "Return from a function or sourced file.";
    "set" => declare::set, "set [-abefhkmnptuvxBCEHPT] [-o option] [--] [argument ...]", "Set options and the positional parameters.";
    "shift" => shift, "shift [n]", "Drop positional parameters.";
    "shopt" => declare::shopt, "shopt [-pqsu] [-o] [name ...]", "Set the shell's optional behaviours.";
    "source" => source, "source filename [arguments]", "Run the commands of a file in this shell.";
    "suspend" => job::suspend, "suspend [-f]", "Stop the shell until it is continued.";
    "test" => crate::cond::test_builtin, "test [expression]", "Evaluate a conditional expression.";
    "times" => misc::times, "times", "Show the CPU time the shell and its children used.";
    "trap" => misc::trap, "trap [-lp] [[action] signal ...]", "Run commands when signals come.";
    "true" => colon, "true", "Succeed.";
    "type" => misc::type_, "type [-afptP] name ...", "Say what a name runs.";
    "typeset" => declare::declare, "typeset [-aAfFgiIlnrtux] [-p] [name[=value] ...]", "Set variables' values and attributes.";
    "ulimit" => misc::ulimit, "ulimit [-SHabcdefiklmnpqrstuvxPRT] [limit]", "Show or set resource limits.";
    "umask" => misc::umask, "umask [-p] [-S] [mode]", "Show or set the file creation mask.";
    "unalias" => misc::unalias, "unalias [-a] name ...", "Remove aliases.";
    "unset" => declare::unset, "unset [-f] [-v] [-n] [name ...]", "Remove variables or functions.";
    "wait" => job::wait, "wait [-fn] [-p var] [id ...]", "Wait for jobs to end.";
}

/// POSIX's special builtins.
const SPECIAL: &[&str] = &[
    ".", ":", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

/// The builtin `name`.
pub fn lookup(name: &str) -> Option<Builtin> {
    TABLE.binary_search_by(|b| b.name.cmp(name)).ok().map(|i| TABLE[i])
}

pub fn is_special(name: &str) -> bool {
    SPECIAL.contains(&name)
}

/// Whether `name`'s assignment arguments are expanded as assignments.
pub fn is_declaration(name: &str) -> bool {
    matches!(name, "declare" | "typeset" | "local" | "export" | "readonly")
}

/// Whether the builtin `name` has not been turned off with `enable -n`.
pub fn enabled(shell: &Shell, name: &str) -> bool {
    !shell.disabled.contains(name)
}

// Helpers the builtins share.

/// Writes a builtin's output; false, with a message, if it could not.
pub fn out(shell: &Shell, name: &str, text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    match sys::write_all(1, &text::encode(text)) {
        Ok(()) => true,
        Err(error) => {
            shell.error(&format!("{name}: write error: {}", sys::describe(&error)));
            false
        }
    }
}

/// Reports a misused builtin, with its usage line; status 2.
pub fn usage(shell: &Shell, name: &str, message: &str) -> Res {
    if !message.is_empty() {
        shell.error(&format!("{name}: {message}"));
    }
    if let Some(builtin) = lookup(name) {
        let line = format!("{name}: usage: {}\n", builtin.usage);
        let _ = sys::write_all(2, &text::encode(&line));
    }
    Ok(2)
}

/// A builtin's options, parsed.
#[derive(Debug, Default)]
pub struct Flags {
    /// Letters given with `-`, in order, with their arguments.
    pub on: Vec<(char, Option<String>)>,
    /// Letters given with `+`.
    pub off: Vec<(char, Option<String>)>,
    /// The operands after the options.
    pub rest: Vec<String>,
}

impl Flags {
    pub fn has(&self, letter: char) -> bool {
        self.on.iter().any(|(c, _)| *c == letter)
    }

    pub fn has_off(&self, letter: char) -> bool {
        self.off.iter().any(|(c, _)| *c == letter)
    }

    /// The argument of the last `-letter`.
    pub fn value(&self, letter: char) -> Option<&str> {
        self.on.iter().rev().find(|(c, _)| *c == letter).and_then(|(_, v)| v.as_deref())
    }

    /// Every argument given with `-letter`, in order.
    pub fn values(&self, letter: char) -> Vec<&str> {
        self.on.iter().filter(|(c, _)| *c == letter).filter_map(|(_, v)| v.as_deref()).collect()
    }
}

/// Parses `args` (after the command name) by `spec`: letters, each
/// followed by `:` if it takes an argument. With `plus`, `+x` is the
/// opposite of `-x`. Options end at `--`, `-`, or the first operand. A
/// bad option is reported; the error is the status to return.
pub fn flags(
    shell: &Shell,
    name: &str,
    args: &[String],
    spec: &str,
    plus: bool,
) -> Result<Flags, i32> {
    let mut flags = Flags::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        let sign = arg.chars().next();
        let is_option = arg.len() > 1 && (sign == Some('-') || plus && sign == Some('+'));
        if !is_option {
            break;
        }
        let on = sign == Some('-');
        let letters: Vec<char> = arg.chars().skip(1).collect();
        let mut j = 0;
        while j < letters.len() {
            let c = letters[j];
            let Some(at) = spec.find(c).filter(|_| c != ':') else {
                let _ = usage(
                    shell,
                    name,
                    &format!("{}{c}: invalid option", if on { '-' } else { '+' }),
                );
                return Err(2);
            };
            let takes = spec[at + c.len_utf8()..].starts_with(':');
            let value = if takes {
                let rest: String = letters[j + 1..].iter().collect();
                j = letters.len();
                if !rest.is_empty() {
                    Some(rest)
                } else if i + 1 < args.len() {
                    i += 1;
                    Some(args[i].clone())
                } else {
                    let _ = usage(shell, name, &format!("-{c}: option requires an argument"));
                    return Err(2);
                }
            } else {
                j += 1;
                None
            };
            if on {
                flags.on.push((c, value));
            } else {
                flags.off.push((c, value));
            }
        }
        i += 1;
    }
    flags.rest = args[i..].to_vec();
    Ok(flags)
}

/// Parses a builtin's count argument (`break 2`, `shift 3`).
fn count(shell: &Shell, name: &str, arg: Option<&String>) -> Result<i64, i32> {
    match arg {
        None => Ok(1),
        Some(text) => match legal_number(text) {
            Some(n) => Ok(n),
            None => {
                shell.error(&format!("{name}: {text}: numeric argument required"));
                Err(2)
            }
        },
    }
}

/// A status argument (`exit 3`, `return 1`), as a byte: a decimal number
/// of 64 bits, as bash reads one, or in POSIX mode, as dash, one from 0
/// to 2^31 - 1.
fn status_arg(shell: &Shell, name: &str, arg: Option<&String>) -> Result<Option<i32>, i32> {
    let Some(text) = arg else { return Ok(None) };
    match legal_number(text) {
        Some(n) if !shell.posix() || (0..=i64::from(i32::MAX)).contains(&n) => {
            Ok(Some((n & 0xff) as i32))
        }
        _ => {
            shell.error(&format!("{name}: {text}: numeric argument required"));
            Err(2)
        }
    }
}

/// A decimal number as bash's `legal_number` reads one: white space before
/// it, blanks after, a sign, and no more than 64 bits; not `010` as octal
/// or `0x10` at all.
pub fn legal_number(text: &str) -> Option<i64> {
    let text = text.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\x0b');
    let text = text.trim_end_matches([' ', '\t']);
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

// The builtins of this file.

fn colon(_: &mut Shell, _: &[String]) -> Res {
    Ok(0)
}

fn false_(_: &mut Shell, _: &[String]) -> Res {
    Ok(1)
}

fn break_(shell: &mut Shell, args: &[String]) -> Res {
    leave_loops(shell, args, true)
}

fn continue_(shell: &mut Shell, args: &[String]) -> Res {
    leave_loops(shell, args, false)
}

fn leave_loops(shell: &mut Shell, args: &[String], is_break: bool) -> Res {
    let name = &args[0];
    // A count that is no number ends the script: with 128 as bash ends
    // it, or as dash with 2, which also ends it for a count below 1.
    let n = match count(shell, name, args.get(1)) {
        Ok(n) => n,
        Err(status) => return Err(Flow::Fatal(if shell.posix() { status } else { 128 })),
    };
    if n < 1 && shell.posix() {
        shell.error(&format!("{name}: {n}: loop count out of range"));
        return Err(Flow::Fatal(2));
    }
    if shell.loops == 0 {
        if !shell.posix() {
            shell.error(&format!("{name}: only meaningful in a `for', `while', or `until' loop"));
        }
        return Ok(0);
    }
    // The loops left end with break's own status, as in bash: 0, or 1 for
    // a count out of range, which leaves every loop.
    if n < 1 {
        shell.error(&format!("{name}: {n}: loop count out of range"));
        shell.status = 1;
        return Err(Flow::Break(shell.loops));
    }
    shell.status = 0;
    let n = u32::try_from(n).unwrap_or(u32::MAX).min(shell.loops);
    Err(if is_break { Flow::Break(n) } else { Flow::Continue(n) })
}

fn return_(shell: &mut Shell, args: &[String]) -> Res {
    if shell.frames.is_empty() {
        shell.error("return: can only `return' from a function or sourced script");
        return Ok(2);
    }
    let status = match status_arg(shell, "return", args.get(1)) {
        Ok(status) => {
            one_operand(shell, "return", args)?;
            status.unwrap_or(shell.status)
        }
        Err(status) => status,
    };
    Err(Flow::Return(status))
}

/// bash's check that `exit` and `return` have one operand at most: more
/// abandon the command. dash takes the first.
fn one_operand(shell: &mut Shell, name: &str, args: &[String]) -> Result<(), Flow> {
    if args.len() > 2 && !shell.posix() {
        return Err(shell.discard(&format!("{name}: too many arguments")));
    }
    Ok(())
}

fn exit(shell: &mut Shell, args: &[String]) -> Res {
    let status = match status_arg(shell, "exit", args.get(1)) {
        Ok(status) => {
            one_operand(shell, "exit", args)?;
            status.or(shell.trap_status).unwrap_or(shell.status)
        }
        Err(status) => status,
    };
    if shell.interactive && shell.subshell == 0 {
        // Stopped jobs are reported once before the shell leaves them.
        let stopped = shell.jobs.list.iter().any(|j| j.stopped());
        if stopped && !shell.exit_warned {
            shell.exit_warned = true;
            let _ = sys::write_all(2, b"There are stopped jobs.\n");
            return Ok(1);
        }
        if shell.login {
            let _ = sys::write_all(2, b"logout\n");
        } else {
            let _ = sys::write_all(2, b"exit\n");
        }
    }
    Err(Flow::Exit(status))
}

fn logout(shell: &mut Shell, args: &[String]) -> Res {
    if !shell.login {
        shell.error("logout: not login shell: use `exit'");
        return Ok(1);
    }
    exit(shell, args)
}

fn shift(shell: &mut Shell, args: &[String]) -> Res {
    // A count that is no number fails shift, as in bash; it ends a POSIX
    // script, as dash's.
    let n = match count(shell, "shift", args.get(1)) {
        Ok(n) => n,
        Err(status) if shell.posix() => return Err(Flow::Fatal(status)),
        Err(_) => return Ok(1),
    };
    if n < 0 {
        shell.error(&format!("shift: {n}: shift count out of range"));
        return Ok(1);
    }
    let n = n as usize;
    if n > shell.args.len() {
        // A special builtin's error, which ends a POSIX script.
        if shell.posix() {
            shell.error(&format!("shift: {n}: shift count out of range"));
            return Err(Flow::Fatal(1));
        }
        if shell.shopt("shift_verbose") {
            shell.error(&format!("shift: {n}: shift count out of range"));
        }
        return Ok(1);
    }
    shell.args.drain(..n);
    Ok(0)
}

fn let_(shell: &mut Shell, args: &[String]) -> Res {
    if args.len() < 2 {
        shell.error("let: expression expected");
        return Ok(1);
    }
    let mut last = 0;
    for expression in &args[1..] {
        match shell.arith_command("let", expression) {
            Some(value) => last = value,
            None => return Ok(1),
        }
    }
    Ok(i32::from(last == 0))
}

fn eval(shell: &mut Shell, args: &[String]) -> Res {
    let text = args[1..].join(" ");
    if text.trim().is_empty() {
        return Ok(0);
    }
    shell.syntax_failed = false;
    let source = shell.source.clone();
    let result = shell.eval_text(&text, &source, "eval");
    syntax_result(shell, result)
}

/// A syntax error in text `eval` or `.` read ends a POSIX shell's script
/// (the builtins are special); otherwise the builtin fails with 2 and the
/// script goes on, as in bash.
fn syntax_result(shell: &mut Shell, result: Res) -> Res {
    if std::mem::take(&mut shell.syntax_failed) {
        if shell.posix() && !shell.interactive {
            return Err(Flow::Exit(2));
        }
        if let Err(Flow::Fatal(status)) = result {
            return Ok(status);
        }
    }
    result
}

/// Where `.` finds `name`: as given if it has a `/`, else on `PATH` (the
/// first readable file by that name, as POSIX has it), then (outside POSIX
/// mode) in the working directory.
fn find_sourced(shell: &Shell, name: &str) -> Option<String> {
    if name.contains('/') {
        return Some(name.to_string());
    }
    if shell.shopt("sourcepath") {
        let path = shell.get("PATH").unwrap_or_default();
        for dir in path.split(':') {
            let dir = if dir.is_empty() { "." } else { dir };
            let candidate = format!("{}/{name}", dir.trim_end_matches('/'));
            if std::fs::metadata(text::to_os(&candidate)).is_ok_and(|m| m.is_file())
                && sys::access(&candidate, libc::R_OK)
            {
                return Some(candidate);
            }
        }
    }
    (!shell.posix()).then(|| name.to_string())
}

/// Reads and runs a startup file, as `.` does.
pub fn source_file(shell: &mut Shell, path: &str) -> Res {
    source(shell, &[".".to_string(), path.to_string()])
}

fn source(shell: &mut Shell, args: &[String]) -> Res {
    let name = &args[0];
    let Some(file) = args.get(1) else {
        shell.error(&format!("{name}: filename argument required"));
        return usage(shell, name, "");
    };
    if shell.opts.restricted && file.contains('/') {
        shell.error(&format!("{name}: {file}: restricted"));
        return Ok(1);
    }
    // A file that cannot be read ends a POSIX shell's script, as `.` is a
    // special builtin; in bash, `.` fails with 1 and the script goes on.
    let failed = |shell: &mut Shell, message: String| {
        shell.error(&message);
        if shell.posix() { Err(Flow::Fatal(1)) } else { Ok(1) }
    };
    let path = find_sourced(shell, file);
    let bytes = match path.as_deref().map(|p| std::fs::read(text::to_os(p))) {
        Some(Ok(bytes)) => bytes,
        Some(Err(error)) if error.raw_os_error() == Some(libc::EISDIR) => {
            return failed(shell, format!("{name}: {file}: is a directory"));
        }
        Some(Err(error)) => {
            let message = match error.raw_os_error() {
                Some(code) => sys::describe(&std::io::Error::from_raw_os_error(code)),
                None => error.to_string(),
            };
            return failed(shell, format!("{file}: {message}"));
        }
        None => return failed(shell, format!("{name}: {file}: file not found")),
    };
    let path = path.unwrap_or_default();
    let new_args = (args.len() > 2).then(|| args[2..].to_vec());
    let saved_args = new_args.map(|new| std::mem::replace(&mut shell.args, new));
    let file_name: Rc<str> = Rc::from(path.as_str());
    let saved_source = std::mem::replace(&mut shell.source, file_name.clone());
    let saved_line = shell.lineno;
    shell.frames.push(Frame {
        name: "source".into(),
        file: file_name.clone(),
        call_line: saved_line,
        saved_opts: None,
    });
    shell.syntax_failed = false;
    let result = shell.run_text(&text::decode(&bytes), file_name, None);
    shell.frames.pop();
    shell.source = saved_source;
    shell.lineno = saved_line;
    if let Some(saved) = saved_args {
        shell.args = saved;
    }
    let result = match result {
        Err(Flow::Return(status)) => Ok(status),
        other => other,
    };
    syntax_result(shell, result)
}

fn exec(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "exec", &args[1..], "cla:", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let Some(name) = flags.rest.first() else { return Ok(0) };
    if shell.opts.restricted {
        shell.error("exec: restricted");
        return Ok(1);
    }
    let path = if name.contains('/') { Some(name.clone()) } else { shell.find_program(name) };
    let Some(path) = path else {
        shell.error(&format!("exec: {name}: not found"));
        return if shell.interactive || shell.shopt("execfail") {
            Ok(127)
        } else {
            Err(Flow::Exit(127))
        };
    };
    let mut argv = flags.rest.clone();
    if let Some(arg0) = flags.value('a') {
        argv[0] = arg0.to_string();
    }
    if flags.has('l') {
        argv[0] = format!("-{}", argv[0]);
    }
    shell.save_history();
    shell.reset_signals_for_exec();
    let c_argv: Vec<_> = argv.iter().map(|a| text::cstring(a)).collect();
    let envp: Vec<_> = if flags.has('c') {
        Vec::new()
    } else {
        shell.environment().iter().map(|e| text::cstring(e)).collect()
    };
    let error = sys::execve(&text::cstring(&path), &c_argv, &envp);
    if error.raw_os_error() == Some(libc::ENOEXEC) {
        let status = shell.run_script_file(&path, &flags.rest[1..]);
        sys::exit_now(status);
    }
    shell.install_signals();
    shell.error(&format!("exec: {name}: {}", sys::describe(&error)));
    let status = if error.raw_os_error() == Some(libc::ENOENT) { 127 } else { 126 };
    if shell.interactive || shell.shopt("execfail") { Ok(status) } else { Err(Flow::Exit(status)) }
}

fn builtin(shell: &mut Shell, args: &[String]) -> Res {
    let Some(name) = args.get(1) else { return Ok(0) };
    match lookup(name).filter(|_| enabled(shell, name)) {
        Some(builtin) => {
            let fields = args[1..].to_vec();
            if is_declaration(name) {
                return run_declaration(
                    shell,
                    name,
                    fields[1..].iter().cloned().map(DeclArg::Plain).collect(),
                );
            }
            (builtin.run)(shell, &fields)
        }
        None => {
            shell.error(&format!("builtin: {name}: not a shell builtin"));
            Ok(1)
        }
    }
}

/// `command`: runs a builtin or program, never a function; with `-v` or
/// `-V`, says what a name is.
fn command(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "command", &args[1..], "pvV", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('v') || flags.has('V') {
        let mut status = 0;
        for name in &flags.rest {
            let found = if flags.has('V') {
                misc::describe(shell, name, misc::Describe::Verbose, flags.has('p'))
            } else {
                misc::describe(shell, name, misc::Describe::Short, flags.has('p'))
            };
            if !found {
                if flags.has('V') {
                    shell.error(&format!("command: {name}: not found"));
                }
                status = 1;
            }
        }
        return Ok(status);
    }
    let Some(name) = flags.rest.first().cloned() else { return Ok(0) };
    if let Some(builtin) = lookup(&name).filter(|_| enabled(shell, &name)) {
        let result = if is_declaration(&name) {
            run_declaration(
                shell,
                &name,
                flags.rest[1..].iter().cloned().map(DeclArg::Plain).collect(),
            )
        } else {
            (builtin.run)(shell, &flags.rest)
        };
        // A special builtin's error does not end the shell under `command`:
        // `command readonly x=1` of a read-only x only fails.
        return match result {
            Err(Flow::Fatal(status)) if is_special(&name) => Ok(status),
            other => other,
        };
    }
    if flags.has('p') {
        let saved = shell.get("PATH");
        shell.set_var("PATH", misc::DEFAULT_PATH);
        shell.hash.clear();
        let result = shell.run_program_fields(flags.rest.clone());
        match saved {
            Some(path) => shell.set_var("PATH", &path),
            None => {
                shell.vars.unset("PATH");
            }
        }
        shell.hash.clear();
        return result;
    }
    shell.run_program_fields(flags.rest.clone())
}

/// The trap and signal state a shell starts with: an interactive shell
/// catches SIGINT and ignores SIGQUIT and SIGTERM (and, with job control,
/// the stop signals); every trap set is installed.
impl Shell {
    pub fn install_signals(&mut self) {
        for (_, signal) in trap::SIGNALS {
            let slot = *signal as usize;
            if matches!(*signal, libc::SIGKILL | libc::SIGSTOP) || self.ignored[slot] {
                continue;
            }
            self.apply_disposition(*signal);
        }
    }

    /// Sets what `signal` does, from its trap and the shell's own needs.
    pub fn apply_disposition(&self, signal: i32) {
        let slot = signal as usize;
        let disposition = match self.traps.get(slot).and_then(|t| t.as_deref()) {
            Some("") => trap::Disposition::Ignore,
            Some(_) => trap::Disposition::Catch,
            None if self.interactive => match signal {
                libc::SIGINT | libc::SIGWINCH | libc::SIGHUP => trap::Disposition::Catch,
                libc::SIGQUIT | libc::SIGTERM => trap::Disposition::Ignore,
                libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU if self.opts.monitor => {
                    trap::Disposition::Ignore
                }
                _ => trap::Disposition::Default,
            },
            None => trap::Disposition::Default,
        };
        trap::set(signal, disposition);
    }
}
