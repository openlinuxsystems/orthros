// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Nukesh, Dukovo's shell.
//!
//! It runs bash's scripts, and POSIX scripts as `/bin/sh` (the POSIX mode
//! it takes when it is run by the name `sh`, or with `--posix`), and it is
//! the interactive shell: job control, the history, programmable
//! completion and a line editor. nukesh(1) has the options; the handbook's
//! chapter on the shell has the rest.
//!
//! ```text
//! nukesh [--posix] [--login] [--norc] [--noprofile] [--rcfile file]
//!        [-abefhimnprstuvxBCEHPT] [-o option] [-O shopt]
//!        [-c command [name [argument...]] | -s [argument...] | [script [argument...]]]
//! nukesh --colors [--check] [file...]
//! ```

mod abbr;
mod arith;
mod assist;
mod ast;
mod brace;
mod builtins;
mod colors;
mod complete;
mod cond;
mod coproc;
mod editor;
mod exec;
mod expand;
mod folders;
#[cfg(test)]
mod fuzz;
mod glob;
mod highlight;
mod history;
mod interactive;
mod jobs;
mod notify;
mod parse;
mod pattern;
mod prompt;
mod record;
mod redir;
mod script;
mod shell;
mod show;
mod suggest;
mod sys;
mod text;
mod trap;
mod vars;

use std::rc::Rc;

use crate::shell::{Flow, Shell};

/// How the shell was started.
#[derive(Default)]
struct Invocation {
    /// `-c`: the first operand is the command.
    command: bool,
    /// `-s`: the commands come on standard input, and the operands are the
    /// positional parameters.
    stdin: bool,
    /// `-i`.
    interactive: bool,
    login: bool,
    restricted: bool,
    posix: bool,
    norc: bool,
    noprofile: bool,
    rcfile: Option<String>,
    /// `set -o` options in the order given, with `-` (true) or `+`.
    options: Vec<(String, bool)>,
    /// `-O` and `+O`.
    shopts: Vec<(String, bool)>,
    operands: Vec<String>,
}

const USAGE: &str = "usage: nukesh [--posix] [--login] [--norc] [--noprofile] [--rcfile file]
              [-abefhimnprstuvxBCEHPT] [-o option] [-O shopt]
              [-c command [name [argument...]] | -s [argument...] | [script [argument...]]]
       nukesh --colors [--check] [file...]
";

fn main() {
    trap::restore_sigpipe();
    let args: Vec<String> = std::env::args_os().map(|a| text::from_os(&a)).collect();
    let status = run(&args);
    sys::exit_now(status & 0xff);
}

fn run(args: &[String]) -> i32 {
    let arg0 = args.first().cloned().unwrap_or_else(|| "nukesh".into());
    let base = arg0.rsplit('/').next().unwrap_or(&arg0).to_string();
    let name = base.trim_start_matches('-').to_string();
    let mut invocation = Invocation {
        login: arg0.starts_with('-'),
        posix: name == "sh",
        restricted: name == "rnukesh",
        ..Invocation::default()
    };
    if let Err(message) = parse_args(args, &mut invocation) {
        let _ = sys::write_all(2, text::encode(&format!("{name}: {message}\n{USAGE}")).as_slice());
        return 2;
    }
    let posix = invocation.posix || invocation.options.iter().any(|(o, on)| o == "posix" && *on);
    let mut shell = Shell::new(&name, posix);
    shell.arg0.clone_from(&arg0);
    // The shell's own file, as bash sets BASH, for the scripts that run
    // `"$BASH" -c` or `exec "$BASH" "$0"`; not in POSIX mode, as in dash.
    if !posix {
        let own = shell.own_path(&arg0);
        shell.set_var("BASH", &own);
    }

    // What to run, and the positional parameters.
    let mut operands = invocation.operands.clone().into_iter();
    let mut command_text = None;
    let mut script = None;
    if invocation.command {
        let Some(text) = operands.next() else {
            let _ = sys::write_all(
                2,
                text::encode(&format!("{name}: -c: option requires an argument\n")).as_slice(),
            );
            return 2;
        };
        command_text = Some(text);
        if let Some(zero) = operands.next() {
            shell.arg0 = zero;
        }
    } else if !invocation.stdin
        && let Some(path) = operands.next()
    {
        shell.arg0.clone_from(&path);
        script = Some(path);
    }
    shell.args = operands.collect();
    shell.command_text = command_text.is_some();
    shell.source_flag = match (&command_text, &script) {
        // dash, which POSIX mode follows, has no `c`.
        (Some(_), _) => (!posix).then_some('c'),
        (None, None) => Some('s'),
        (None, Some(_)) => None,
    };

    let interactive = invocation.interactive
        || (command_text.is_none() && script.is_none() && sys::isatty(0) && sys::isatty(2));
    shell.interactive = interactive;
    shell.login = invocation.login;
    shell.shopts.insert("login_shell", invocation.login);
    shell.source = Rc::from(if script.is_some() || command_text.is_some() {
        shell.arg0.as_str()
    } else {
        name.as_str()
    });
    if !interactive {
        // A script's `[ -z "$PS1" ]` tells that it is not interactive.
        for prompt in ["PS1", "PS2"] {
            if std::env::var_os(prompt).is_none() {
                shell.vars.unset(prompt);
            }
        }
    }

    // Signals ignored when the shell started stay so in a script.
    for (_, signal) in trap::SIGNALS {
        shell.ignored[*signal as usize] = !interactive && trap::ignored_at_start(*signal);
    }
    // The line editor's keys, aliases and appending to the history file are
    // for a shell typed at, as bash (and Debian's bash.bashrc) has them.
    shell.opts.emacs = interactive;
    shell.shopts.insert("expand_aliases", interactive);
    shell.shopts.insert("histappend", interactive);
    if posix {
        shell.shopts.insert("inherit_errexit", true);
    }
    if interactive {
        shell.opts.monitor = true;
        shell.history_option("history", true);
        shell.history_option("histexpand", !posix);
        if shell.get("HISTFILE").is_none()
            && let Some(home) = shell.get("HOME")
        {
            shell.set_var("HISTFILE", &format!("{}/.nukesh_history", home.trim_end_matches('/')));
        }
        // The colours, and the tools' colours, before the startup files,
        // which may change them.
        let (colors, problems) = colors::Colors::load(shell.get("HOME").as_deref());
        for problem in &problems {
            shell
                .error(&format!("{}: {} (nukesh --colors --check)", problem.file, problem.message));
        }
        shell.colors = Rc::new(colors);
        colors::color_the_tools(&mut shell);
        // The command record every shell of the person's shares (ADR 0262).
        shell.records = Some(record::Records::new(shell.get("HOME").as_deref()));
    }
    for (option, on) in &invocation.options {
        if builtins::set_option(&mut shell, option, *on).is_err() {
            return 2;
        }
    }
    for (option, on) in &invocation.shopts {
        match shell.shopts.get_mut(option.as_str()) {
            Some(value) => *value = *on,
            None => {
                shell.error(&format!("{option}: invalid shell option name"));
                return 2;
            }
        }
    }
    if interactive && shell.opts.monitor && shell.tty.is_none() {
        interactive::take_terminal_control(&mut shell);
    }
    shell.install_signals();
    if interactive {
        shell.update_window_size();
    }

    if let Err(code) = startup_files(&mut shell, &invocation) {
        return shell.run_exit_trap(code);
    }
    // Restricted mode starts after the startup files.
    if invocation.restricted {
        shell.opts.restricted = true;
    }

    if let Some(text) = command_text {
        let source: Rc<str> = Rc::from(shell.arg0.as_str());
        let result = shell.run_text(&text, source, Some("-c"));
        // bash's -c ends with 127 after an expansion that ends the shell,
        // where a script ends with 1.
        let status = match result {
            Err(Flow::Fatal(_)) if shell.expansion_failed && !shell.posix() => 127,
            result => shell.flow_status(result),
        };
        return shell.run_exit_trap(status);
    }
    if let Some(path) = script {
        let status = run_script(&mut shell, &path);
        return shell.run_exit_trap(status);
    }
    if interactive {
        return interactive::run(shell);
    }
    let source: Rc<str> = Rc::from(name.as_str());
    let result = shell.run_fd(0, source);
    let status = shell.flow_status(result);
    shell.run_exit_trap(status)
}

/// Reads the options, which end at the first operand, `--` or `-`.
fn parse_args(args: &[String], invocation: &mut Invocation) -> Result<(), String> {
    let mut i = 1;
    // Long options come first.
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--posix" => invocation.posix = true,
            "--login" => invocation.login = true,
            "--norc" => invocation.norc = true,
            "--noprofile" => invocation.noprofile = true,
            "--restricted" => invocation.restricted = true,
            "--noediting" => {}
            "--verbose" => invocation.options.push(("verbose".into(), true)),
            "--rcfile" | "--init-file" => {
                i += 1;
                let file =
                    args.get(i).ok_or_else(|| format!("{arg}: option requires an argument"))?;
                invocation.rcfile = Some(file.clone());
            }
            // colors.toml(5): shown, or checked (ADR 0258).
            "--colors" if i == 1 => sys::exit_now(colors::command(&args[2..])),
            "--version" => {
                let version = format!("nukesh, version {}\n", env!("CARGO_PKG_VERSION"));
                let _ = sys::write_all(1, version.as_bytes());
                sys::exit_now(0);
            }
            "--help" => {
                let _ = sys::write_all(1, USAGE.as_bytes());
                sys::exit_now(0);
            }
            "--" => break,
            long if long.starts_with("--") => return Err(format!("{long}: invalid option")),
            _ => break,
        }
        i += 1;
    }
    while let Some(arg) = args.get(i) {
        if arg == "--" || arg == "-" {
            i += 1;
            break;
        }
        let on = match arg.chars().next() {
            Some('-') => true,
            Some('+') => false,
            _ => break,
        };
        if arg.len() == 1 {
            break;
        }
        i += 1;
        for letter in arg.chars().skip(1) {
            match letter {
                'c' if on => invocation.command = true,
                's' if on => invocation.stdin = true,
                'i' if on => invocation.interactive = true,
                'l' if on => invocation.login = true,
                'r' if on => invocation.restricted = true,
                'o' | 'O' => {
                    let option = args
                        .get(i)
                        .ok_or_else(|| format!("-{letter}: option requires an argument"))?;
                    i += 1;
                    if letter == 'o' {
                        invocation.options.push((option.clone(), on));
                    } else {
                        invocation.shopts.push((option.clone(), on));
                    }
                }
                'H' => invocation.options.push(("histexpand".into(), on)),
                _ => match shell::Opts::by_letter(letter) {
                    Some(option) => invocation.options.push((option.into(), on)),
                    None => {
                        return Err(format!(
                            "{}{letter}: invalid option",
                            if on { '-' } else { '+' }
                        ));
                    }
                },
            }
        }
    }
    invocation.operands = args[i.min(args.len())..].to_vec();
    Ok(())
}

/// Reads the startup files: a login shell's profile, an interactive
/// shell's `~/.nukeshrc`, and in POSIX mode the file `ENV` names. An exit
/// from one is the shell's.
fn startup_files(shell: &mut Shell, invocation: &Invocation) -> Result<(), i32> {
    let home = shell.get("HOME").map(|h| h.trim_end_matches('/').to_string());
    let in_home = |name: &str| home.as_ref().map(|home| format!("{home}/{name}"));
    let posix = shell.posix();
    if shell.login && !invocation.noprofile {
        source_if_there(shell, "/etc/profile")?;
        let names: &[&str] =
            if posix { &[".profile"] } else { &[".nukesh_profile", ".nukesh_login", ".profile"] };
        if let Some(path) = names.iter().filter_map(|n| in_home(n)).find(|p| exists(p)) {
            source_if_there(shell, &path)?;
        }
    }
    if shell.interactive && !shell.login && !posix && !invocation.norc {
        match &invocation.rcfile {
            Some(rcfile) => source_if_there(shell, rcfile)?,
            None => {
                source_if_there(shell, "/etc/nukeshrc")?;
                if let Some(path) = in_home(".nukeshrc") {
                    source_if_there(shell, &path)?;
                }
            }
        }
    }
    // Not for a set-user-ID shell, which must not run the user's file.
    let trusted = sys::geteuid() == sys::getuid();
    let env = if shell.interactive && posix {
        "ENV"
    } else if !shell.interactive && !posix {
        "NUKESH_ENV"
    } else {
        return Ok(());
    };
    if trusted && let Some(file) = shell.get(env).filter(|f| !f.is_empty()) {
        let path = match parse::string_word(&file, Rc::from(env), shell.parse_options()) {
            Ok(word) => shell.expand_quoted(&word).unwrap_or(file),
            Err(_) => file,
        };
        source_if_there(shell, &path)?;
    }
    Ok(())
}

fn exists(path: &str) -> bool {
    std::fs::metadata(text::to_os(path)).is_ok()
}

/// Runs a startup file if it is there; errors in it do not stop the shell.
fn source_if_there(shell: &mut Shell, path: &str) -> Result<(), i32> {
    if !exists(path) {
        return Ok(());
    }
    match builtins::source_file(shell, path) {
        Err(Flow::Exit(code)) => Err(code),
        _ => Ok(()),
    }
}

/// Where the script's descriptor goes.
const SCRIPT_FD: i32 = 255;

/// An error before any command runs: the shell's name, then `message`.
fn complain(shell: &Shell, message: &str) {
    let _ = sys::write_all(2, text::encode(&format!("{}: {message}\n", shell.name)).as_slice());
}

/// Runs a script file: found as given, or on `PATH` when its name has no
/// `/` and it is not in the working directory.
fn run_script(shell: &mut Shell, path: &str) -> i32 {
    let mut found = path.to_string();
    if !path.contains('/') && !exists(path) {
        let search = shell.get("PATH").unwrap_or_default();
        if let Some(candidate) = search
            .split(':')
            .map(|dir| {
                format!("{}/{path}", if dir.is_empty() { "." } else { dir.trim_end_matches('/') })
            })
            .find(|candidate| std::fs::metadata(text::to_os(candidate)).is_ok_and(|m| m.is_file()))
        {
            found = candidate;
        }
    }
    let fd = match sys::open(&found, libc::O_RDONLY, 0) {
        Ok(fd) => fd,
        Err(error) => {
            complain(shell, &format!("{path}: {}", sys::describe(&error)));
            return if error.raw_os_error() == Some(libc::ENOENT) { 127 } else { 126 };
        }
    };
    if std::fs::metadata(text::to_os(&found)).is_ok_and(|m| m.is_dir()) {
        complain(shell, &format!("{path}: Is a directory"));
        return 126;
    }
    // A program that is not a script: NUL bytes in its first line.
    let mut head = [0u8; 80];
    let n = sys::read(fd, &mut head).unwrap_or(0);
    let first_line = head[..n].split(|b| *b == b'\n').next().unwrap_or(&[]);
    if first_line.contains(&0) {
        complain(shell, &format!("{path}: cannot execute binary file"));
        return 126;
    }
    let _ = sys::lseek(fd, 0, libc::SEEK_SET);
    // Out of the way of the script's own descriptors, at 255 as in bash.
    let fd = match sys::dup_at_least(fd, SCRIPT_FD).or_else(|_| sys::dup_high(fd)) {
        Ok(high) => {
            sys::close(fd);
            high
        }
        Err(_) => fd,
    };
    shell.in_script = true;
    let result = shell.run_fd(fd, Rc::from(path));
    sys::close(fd);
    shell.flow_status(result)
}
