// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! elevate(8) and sudo(8): running a command as an administrator
//! (ADR 0219).
//!
//! There is no setuid program. elevate asks authd(8), which checks that
//! the asker is an administrator and, unless their terminal pane typed
//! their password in the last 15 minutes, their password; then authd
//! starts the command itself, as its own child, acting for root with the
//! admin set: authd's root, which passes the owners and modes of files
//! (the bypass right), and the platform capability. elevate sends authd
//! the three descriptors the command is to have, and a pipe to pass
//! signals on. A descriptor that is a terminal is not sent as it is: as
//! sudo's `use_pty` does, elevate makes a new pseudo-terminal for the
//! command and copies between it and its own, so that the command and
//! its children are the foreground of a terminal of their own, and Ctrl+C
//! and Ctrl+Z reach them, not the asker's shell. authd says when the
//! command stops and when it ends, and elevate ends with its status.
//!
//! The command is looked for only in [`PATH`] and starts with an
//! environment of its own ([`environment`]), as sudo's `secure_path` and
//! `env_reset` have it: nothing of the asker's environment but the
//! terminal's type and the language settings passes, so that nothing of
//! theirs, such as a command of their own named like a system one, runs
//! as an administrator by accident.
//!
//! authd runs `elevate --started` first in the new process: it makes the
//! terminal it was given its own controlling terminal, goes to the
//! directory asked for, and runs the command.
//!
//! As root, which an elevated command is, elevate runs the command
//! itself, with the same path and environment.

use std::ffi::{CString, OsStr, OsString};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use accounts::User;
use accounts::request::{self, Answer, Mode, NEEDS_PASSWORD, Request, WRONG_PASSWORD};
use radix_abi::capability::{self, ObjectKind, Rights};
use radix_abi::directory::{self, OpenFlags};
use radix_abi::{Status, process, startup};

use crate::args::{Arg, Args};
use crate::sys::{self, SYS};
use crate::util::{self, Fail, fail, fail_with, usage};
use crate::{prompt, signin};

/// The only places an elevated command is looked for, whatever the
/// asker's `PATH` says, as sudo's `secure_path`: the system's commands,
/// then those of the bundles installed for everyone.
pub const PATH: &str = "/system/commands:/applications/commands";

/// How long a terminal pane stays unlocked after its password, from its
/// last elevated command (the owner's choice for Phase 13; sudo's
/// per-terminal default on Ubuntu).
pub const UNLOCKED_FOR: Duration = Duration::from_mins(15);

/// What authd runs first in an elevated process: elevate itself, with
/// [`STARTED`].
const HELPER: &str = "/system/commands/elevate";

/// The hidden option of the first program of an elevated process.
const STARTED: &str = "--started";

/// The most wrong passwords elevate takes before it gives up, as sudo's
/// `passwd_tries`.
const TRIES: u32 = 3;

/// The longest command line the audit log and `SUDO_COMMAND` show.
const SHOWN_MAX: usize = 256;

/// Whether `entry`, `NAME=value`, is a language setting an elevated
/// command keeps: `LANG`, `LANGUAGE` or an `LC_` one, whose value is
/// letters, digits and `._-@:` only, so that it cannot name a file of
/// the asker's as a locale (sudo's `env_check`).
pub fn language_setting(entry: &str) -> bool {
    let Some((name, value)) = entry.split_once('=') else { return false };
    let lc = name
        .strip_prefix("LC_")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase()));
    let named = name == "LANG" || name == "LANGUAGE" || lc;
    let fine = |c: char| c.is_ascii_alphanumeric() || "._-@:".contains(c);
    named && value.len() <= 64 && value.chars().all(fine)
}

/// The language settings of this program's environment that an elevated
/// command keeps.
fn own_language() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(name, value)| {
            let entry = format!("{}={}", name.to_str()?, value.to_str()?);
            language_setting(&entry).then_some(entry)
        })
        .collect()
}

/// `words` as one line for the audit log and `SUDO_COMMAND`: joined by
/// spaces, each control character as `?`, and at most [`SHOWN_MAX`]
/// characters, so that it cannot start a line of its own.
pub fn shown_command(words: &[Vec<u8>]) -> String {
    let line: Vec<String> = words.iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect();
    line.join(" ").chars().take(SHOWN_MAX).map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// `command` as one line for a shell's `-c`, each character the shell
/// would read as more than itself escaped with `\`, as sudo does for
/// `-s` and `-i`.
pub fn shell_line(command: &[Vec<u8>]) -> Vec<u8> {
    let mut line = Vec::new();
    for (i, word) in command.iter().enumerate() {
        if i > 0 {
            line.push(b' ');
        }
        for &byte in word {
            if !(byte.is_ascii_alphanumeric() || byte >= 0x80 || b"_-/.:=+,%@$".contains(&byte)) {
                line.push(b'\\');
            }
            line.push(byte);
        }
    }
    line
}

/// Where `name`, the command an elevated process is to run, is: as
/// given, against `directory` if relative, when it holds a `/`; else the
/// first file of that name in [`PATH`]. None if there is no such file.
pub fn resolve(name: &[u8], directory: &[u8]) -> Option<Vec<u8>> {
    let is_file =
        |path: &[u8]| std::fs::metadata(OsStr::from_bytes(path)).is_ok_and(|m| m.is_file());
    if name.contains(&b'/') {
        let path =
            if name.starts_with(b"/") { name.to_vec() } else { [directory, b"/", name].concat() };
        return is_file(&path).then_some(path);
    }
    if name.is_empty() {
        return None;
    }
    PATH.split(':').map(|dir| [dir.as_bytes(), b"/", name].concat()).find(|path| is_file(path))
}

/// What an elevated process runs.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The program's path.
    pub program: Vec<u8>,
    /// Its arguments, its name first.
    pub argv: Vec<Vec<u8>>,
    /// The directory it starts in.
    pub directory: Vec<u8>,
    /// The line the audit log shows: the program's path and arguments.
    pub shown: String,
}

/// What an elevated process runs for `mode` and `command`, asked from
/// `directory`, with root's account `root`: the command, found by
/// [`resolve`]; or root's shell, running the command with `-c` if there
/// is one, in `directory` or, as a login shell, in root's home.
pub fn plan(
    mode: Mode,
    command: &[Vec<u8>],
    directory: &[u8],
    root: &User,
) -> Result<Plan, String> {
    if mode == Mode::Command {
        let name = command.first().ok_or("no command to run")?;
        let program = resolve(name, directory).ok_or_else(|| {
            format!("{}: command not found", shown_command(std::slice::from_ref(name)))
        })?;
        let mut words = vec![program.clone()];
        words.extend(command[1..].iter().cloned());
        return Ok(Plan {
            program,
            argv: command.to_vec(),
            directory: directory.to_vec(),
            shown: shown_command(&words),
        });
    }
    let shell = root.shell.as_bytes().to_vec();
    let base = shell.rsplit(|&b| b == b'/').next().filter(|b| !b.is_empty()).unwrap_or(b"sh");
    // A login shell's name starts with a dash (sh(1)).
    let argv0 = if mode == Mode::Login { [b"-", base].concat() } else { base.to_vec() };
    let mut argv = vec![argv0];
    if !command.is_empty() {
        argv.push(b"-c".to_vec());
        argv.push(shell_line(command));
    }
    let directory =
        if mode == Mode::Login { root.home.as_bytes().to_vec() } else { directory.to_vec() };
    let mut words = vec![shell.clone()];
    words.extend(argv[1..].iter().cloned());
    Ok(Plan { program: shell, argv, directory, shown: shown_command(&words) })
}

/// The environment an elevated command starts with, as sudo's with
/// `env_reset` and `secure_path` on Ubuntu: root's home, shell and name,
/// [`PATH`], the terminal's type, the asker's language settings, and who
/// asked, in sudo's own variables, which scripts read.
pub fn environment(
    root: &User,
    asker: &User,
    term: &str,
    language: &[String],
    shown: &str,
) -> Vec<String> {
    let mut env = vec![
        format!("HOME={}", root.home),
        format!("SHELL={}", root.shell),
        format!("USER={}", root.name),
        format!("LOGNAME={}", root.name),
        format!("PATH={PATH}"),
        format!("TERM={}", signin::terminal_type(term)),
        format!("SUDO_USER={}", asker.name),
        format!("SUDO_UID={}", asker.uid),
        format!("SUDO_GID={}", asker.gid),
        format!("SUDO_COMMAND={shown}"),
    ];
    env.extend(language.iter().filter(|entry| language_setting(entry)).cloned());
    env
}

/// Root's account in `users`, or the one every image has if it is
/// missing.
pub fn root_account(users: impl Iterator<Item = User>) -> User {
    let mut users = users;
    users.find(|u| u.uid == 0).unwrap_or_else(|| User {
        name: "root".into(),
        uid: 0,
        gid: 0,
        full_name: "root".into(),
        home: "/".into(),
        shell: accounts::SHELL.into(),
    })
}

/// Starts an elevated process for authd: its child, acting for root as
/// authd does, with authd's root, which has the bypass right, the random
/// source, the log, the platform capability if authd holds one, and
/// `fds` as its descriptors 0 to 2. It runs [`HELPER`], which makes the
/// command's terminal its own and runs `plan`'s program. Returns the
/// process's number.
pub fn start(fds: &[OwnedFd], plan: &Plan, env: &[String]) -> Result<u64, String> {
    let failed = |what: &'static str| {
        move |status: Status| format!("cannot elevate: {what}: {}", sys::status_name(status.0))
    };
    let spares =
        signin::Spares::new(2).map_err(|err| format!("cannot elevate: no slots: {err}"))?;
    let [program, child] = [0, 1].map(|i| spares.slot(i));
    let helper = HELPER.trim_start_matches('/');
    directory::open(
        &SYS,
        startup::SLOT_ROOT,
        helper.as_bytes(),
        OpenFlags::NONE,
        Rights::READ,
        program,
        0,
    )
    .map_err(failed("opening elevate"))?;
    process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Process, signin::SLOTS, child)
        .map_err(failed("making its process"))?;
    let mut shares = vec![
        (startup::SLOT_ROOT, startup::SLOT_ROOT),
        (startup::SLOT_RANDOM, startup::SLOT_RANDOM),
        (startup::SLOT_LOG, startup::SLOT_LOG),
    ];
    let platform = capability::info(&SYS, startup::SLOT_PLATFORM);
    if platform.is_ok_and(|info| info.kind == ObjectKind::Platform) {
        shares.push((startup::SLOT_PLATFORM, startup::SLOT_PLATFORM));
    }
    for (fd, n) in fds.iter().zip(0..3) {
        shares.push((sys::slot_of(fd.as_raw_fd()), startup::SLOT_FD_BASE + n));
    }
    for (from, to) in shares {
        process::share(&SYS, child, from, to, Rights::ALL, 0)
            .map_err(failed("giving it the admin set"))?;
    }
    let mut args: Vec<&[u8]> = vec![b"elevate", STARTED.as_bytes(), &plan.directory, &plan.program];
    args.extend(plan.argv.iter().map(Vec::as_slice));
    let block = signin::exec_block(HELPER.as_bytes(), &args, env);
    process::exec(&SYS, child, program, &block).map_err(failed("starting elevate"))?;
    // The child stays in the tree after its capability goes, for wait.
    let number = process::info(&SYS, child).map_err(failed("its number"))?.number;
    Ok(number)
}

/// `elevate --started DIRECTORY PROGRAM NAME [ARGUMENT...]`, the first
/// program of an elevated process: a session of its own, with the first
/// of its descriptors 0 to 2 that is a terminal as its controlling
/// terminal, in DIRECTORY, running PROGRAM.
fn started(words: &[OsString]) -> Result<i32, Fail> {
    let [directory, program, argv @ ..] = words else {
        return Err(usage("usage: elevate --started DIRECTORY PROGRAM NAME [ARGUMENT...]".into()));
    };
    if argv.is_empty() {
        return Err(usage("elevate --started needs the program's name".into()));
    }
    // SAFETY: setsid and the terminal request change only this process's
    // session; authd made it in its own process group, so setsid works.
    unsafe {
        libc::setsid();
        if let Some(fd) = (0..3).find(|&fd| libc::isatty(fd) == 1) {
            libc::ioctl(fd, libc::TIOCSCTTY, 0);
        }
    }
    if let Err(err) = std::env::set_current_dir(directory) {
        eprintln!(
            "elevate: cannot go to {}: {}; starting in /",
            directory.to_string_lossy(),
            util::strip_os_error(&err)
        );
        let _ = std::env::set_current_dir("/");
    }
    let c = |word: &OsString| CString::new(word.as_bytes()).unwrap_or_default();
    let path = c(program);
    let strings: Vec<CString> = argv.iter().map(c).collect();
    let mut pointers: Vec<*const libc::c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    pointers.push(std::ptr::null());
    // SAFETY: execv takes NUL-terminated strings and a null-ended list,
    // which live until it returns, if it does.
    unsafe { libc::execv(path.as_ptr(), pointers.as_ptr()) };
    let err = io::Error::last_os_error();
    let code = if err.kind() == io::ErrorKind::NotFound { 127 } else { 126 };
    Err(fail_with(code, format!("{}: {}", program.to_string_lossy(), util::strip_os_error(&err))))
}

/// What was asked on the command line.
struct Options {
    mode: Mode,
    command: Vec<Vec<u8>>,
    /// -n: fail rather than ask for a password.
    non_interactive: bool,
    /// -S: read the password from the standard input.
    stdin: bool,
    /// -p: the prompt.
    prompt: Option<String>,
    /// -k with a command: ask for the password even if unlocked.
    forget_first: bool,
}

/// What to do.
enum Action {
    Run(Options),
    /// -v: unlock the pane, or keep it unlocked 15 minutes more.
    Unlock(Options),
    /// -k or -K alone: lock the pane again.
    Forget,
    /// -l: say whether the user may elevate.
    List,
}

fn usage_text(tool: &str) -> String {
    format!(
        "usage: {tool} [-nS] [-p PROMPT] [-u root] COMMAND [ARGUMENT...]\n       {tool} [-nS] [-p PROMPT] -s|-i [COMMAND [ARGUMENT...]]\n       {tool} -v | -k | -K | -l | -V | -h"
    )
}

fn options(args: &mut Args) -> Result<Action, Fail> {
    args.stop_at_operand();
    let tool = args.tool;
    let mut options = Options {
        mode: Mode::Command,
        command: Vec::new(),
        non_interactive: false,
        stdin: false,
        prompt: None,
        forget_first: false,
    };
    let (mut validate, mut forget, mut list) = (false, false, false);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('s') => options.mode = Mode::Shell,
            Arg::Long(l) if l == "shell" => options.mode = Mode::Shell,
            Arg::Short('i') => options.mode = Mode::Login,
            Arg::Long(l) if l == "login" => options.mode = Mode::Login,
            Arg::Short('n') => options.non_interactive = true,
            Arg::Long(l) if l == "non-interactive" => options.non_interactive = true,
            Arg::Short('S') => options.stdin = true,
            Arg::Long(l) if l == "stdin" => options.stdin = true,
            Arg::Short('p') => options.prompt = Some(args.text()?),
            Arg::Long(l) if l == "prompt" => options.prompt = Some(args.text()?),
            Arg::Short('v') => validate = true,
            Arg::Long(l) if l == "validate" => validate = true,
            Arg::Short('k' | 'K') => forget = true,
            Arg::Long(l) if l == "reset-timestamp" || l == "remove-timestamp" => forget = true,
            Arg::Short('l') => list = true,
            Arg::Long(l) if l == "list" => list = true,
            // The home is always root's, as with sudo's -H.
            Arg::Short('H') => {}
            Arg::Long(l) if l == "set-home" => {}
            Arg::Short('u') => user(&args.text()?)?,
            Arg::Long(l) if l == "user" => user(&args.text()?)?,
            Arg::Short('E') => return Err(keeps_clean()),
            Arg::Long(l) if l == "preserve-env" => return Err(keeps_clean()),
            Arg::Short('V') => {
                println!("{tool} (Dukovo) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Arg::Long(l) if l == "version" => {
                println!("{tool} (Dukovo) {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Arg::Short('h') => {
                println!("{}", usage_text(tool));
                std::process::exit(0);
            }
            Arg::Long(l) if l == "help" => {
                println!("{}", usage_text(tool));
                std::process::exit(0);
            }
            Arg::Operand(word) => {
                options.command.push(word.into_vec());
                options.command.extend(args.rest().into_iter().map(OsString::into_vec));
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if list {
        return Ok(Action::List);
    }
    if validate {
        return Ok(Action::Unlock(options));
    }
    if forget && options.command.is_empty() && options.mode == Mode::Command {
        return Ok(Action::Forget);
    }
    options.forget_first = forget;
    if options.mode == Mode::Command && options.command.is_empty() {
        return Err(usage(usage_text(tool)));
    }
    Ok(Action::Run(options))
}

/// Takes `-u NAME`: only root, whose admin set elevate gives.
fn user(name: &str) -> Result<(), Fail> {
    if name == "root" || name == "#0" || name == "0" {
        return Ok(());
    }
    Err(fail(format!(
        "elevate runs commands only as an administrator, root, not as {name}; a service's own \
         account comes with the service (Phase 13d)"
    )))
}

fn keeps_clean() -> Fail {
    fail(
        "an elevated command keeps none of your environment but the terminal's type and the \
         language; give it what it needs with env(1): elevate env NAME=value COMMAND"
            .into(),
    )
}

/// elevate(8) and sudo(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut words = args.rest();
    if words.first().is_some_and(|w| w == STARTED) {
        words.remove(0);
        return started(&words);
    }
    let mut args = Args::new(args.tool, words);
    let action = options(&mut args)?;
    if util::own_user() == 0 {
        return as_root(action);
    }
    match action {
        Action::List => list(),
        Action::Forget => ask(&Request::Forget, &[]).and_then(|answer| done(answer).map(|_| 0)),
        Action::Unlock(options) => unlock(&options),
        Action::Run(options) => {
            if options.forget_first {
                done(ask(&Request::Forget, &[])?)?;
            }
            run(&options)
        }
    }
}

/// The account of user `uid`, from the passwd file.
fn account(uid: u32) -> Result<User, Fail> {
    let text = std::fs::read_to_string(accounts::PASSWD)
        .map_err(|e| util::failed(&accounts::PASSWD, e))?;
    let users = accounts::users(&text);
    let found = users.entries().find(|u| u.uid == uid).cloned();
    found.ok_or_else(|| fail(format!("user {uid} has no account in {}", accounts::PASSWD)))
}

/// -l: whether the user may elevate, from the account files.
fn list() -> Result<i32, Fail> {
    let me = account(util::own_user())?;
    let groups =
        std::fs::read_to_string(accounts::GROUP).map_err(|e| util::failed(&accounts::GROUP, e))?;
    if accounts::is_admin(&me, &accounts::groups(&groups)) {
        println!(
            "User {} may run any command as an administrator with elevate, after typing their \
             password once in each terminal pane every 15 minutes.",
            me.name
        );
        Ok(0)
    } else {
        println!(
            "User {} is not an administrator, and may not elevate: only members of the {} group \
             may.",
            me.name,
            accounts::ADMINS
        );
        Ok(1)
    }
}

/// What root does: runs the command itself, with the same path and
/// environment as an elevated one; there is nothing to unlock.
fn as_root(action: Action) -> Result<i32, Fail> {
    let options = match action {
        Action::Run(options) => options,
        Action::Unlock(_) | Action::Forget => return Ok(0),
        Action::List => {
            println!("User root may run any command: it is an administrator already.");
            return Ok(0);
        }
    };
    let root = root_account(account(0).into_iter());
    let here =
        std::env::current_dir().map_or_else(|_| b"/".to_vec(), |d| d.into_os_string().into_vec());
    let plan = plan(options.mode, &options.command, &here, &root).map_err(fail)?;
    let term = std::env::var("TERM").unwrap_or_default();
    let env = environment(&root, &root, &term, &own_language(), &plan.shown);
    if let Err(err) = std::env::set_current_dir(OsStr::from_bytes(&plan.directory)) {
        eprintln!(
            "elevate: cannot go to {}: {}; starting in /",
            String::from_utf8_lossy(&plan.directory),
            util::strip_os_error(&err)
        );
        let _ = std::env::set_current_dir("/");
    }
    let c = |bytes: &[u8]| CString::new(bytes).unwrap_or_default();
    let path = c(&plan.program);
    let argv: Vec<CString> = plan.argv.iter().map(|w| c(w)).collect();
    let envp: Vec<CString> = env.iter().map(|e| c(e.as_bytes())).collect();
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|s| s.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> = envp.iter().map(|s| s.as_ptr()).collect();
    envp_p.push(std::ptr::null());
    // SAFETY: execve takes NUL-terminated strings and null-ended lists,
    // which live until it returns, if it does.
    unsafe { libc::execve(path.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr()) };
    let err = io::Error::last_os_error();
    let code = if err.kind() == io::ErrorKind::NotFound { 127 } else { 126 };
    Err(fail_with(
        code,
        format!("{}: {}", String::from_utf8_lossy(&plan.program), util::strip_os_error(&err)),
    ))
}

/// Connects to authd.
fn connect() -> Result<UnixStream, Fail> {
    UnixStream::connect(accounts::SOCKET).map_err(|err| {
        fail(format!("cannot reach authd at {}: {}", accounts::SOCKET, util::strip_os_error(&err)))
    })
}

/// Asks authd `request`, with the descriptors `fds`; the answer, and the
/// connection to read more from.
fn ask_with(request: &Request, fds: &[RawFd]) -> Result<(Answer, BufReader<UnixStream>), Fail> {
    let stream = connect()?;
    let mut bytes = request.encode();
    let sent = if fds.is_empty() {
        (&stream).write_all(&bytes)
    } else {
        signin::send_with_fds(&stream, &bytes, fds)
    };
    accounts::wipe(&mut bytes);
    sent.and_then(|()| stream.shutdown(Shutdown::Write))
        .map_err(|err| fail(format!("cannot ask authd: {}", util::strip_os_error(&err))))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| fail(format!("authd did not answer: {}", util::strip_os_error(&err))))?;
    Ok((Answer::decode(&line), reader))
}

fn ask(request: &Request, fds: &[RawFd]) -> Result<Answer, Fail> {
    ask_with(request, fds).map(|(answer, _)| answer)
}

/// What an answer said, or the refusal as a failure.
fn done(answer: Answer) -> Result<String, Fail> {
    match answer {
        Answer::Done(text) => Ok(text),
        Answer::Refused(why) => Err(fail(why)),
    }
}

/// Asks for the password, as `prompt` or `[elevate] password for NAME: `:
/// on the terminal, with its echo off, or from the standard input with
/// -S.
fn password_typed(options: &Options, tool: &str) -> Result<Vec<u8>, Fail> {
    if options.non_interactive {
        return Err(fail("a password is needed, and -n says not to ask for one".into()));
    }
    let me = util::user_name(util::own_user());
    let text = match &options.prompt {
        Some(prompt) => prompt
            .replace("%p", &me)
            .replace("%u", &me)
            .replace("%h", &util::hostname())
            .replace("%%", "%"),
        None => format!("[{tool}] password for {me}: "),
    };
    let answer = if options.stdin {
        prompt::ask_on(0, 2, &text, true)
    } else {
        let tty = std::fs::OpenOptions::new().read(true).write(true).open("/devices/tty").map_err(
            |_| {
                fail(
                    "a terminal is needed to type the password, or -S to read it from the \
                      standard input"
                        .into(),
                )
            },
        )?;
        prompt::ask_on(tty.as_raw_fd(), tty.as_raw_fd(), &text, false)
    };
    match answer {
        Ok(prompt::Answer::Line(line)) => Ok(line),
        Ok(prompt::Answer::Stopped) => Err(fail("no password was typed".into())),
        Err(err) => Err(fail(format!("cannot read the password: {}", util::strip_os_error(&err)))),
    }
}

/// -v: unlocks the pane, asking for the password if it is locked.
fn unlock(options: &Options) -> Result<i32, Fail> {
    let tool = tool_name();
    let mut typed: Option<Vec<u8>> = None;
    let mut console = console_asks(options);
    let mut wrong = 0;
    loop {
        let password = typed.take();
        let asks = console && password.is_none();
        let request = Request::Unlock { password, ask: asks };
        match ask(&request, &[])? {
            Answer::Done(text) if text == NEEDS_PASSWORD => {
                console = false;
                typed = Some(password_typed(options, &tool)?);
            }
            Answer::Done(_) => return Ok(0),
            Answer::Refused(why) if why == WRONG_PASSWORD => {
                wrong += 1;
                if wrong >= TRIES {
                    return Err(fail(format!("{TRIES} wrong passwords")));
                }
                eprintln!("Sorry, try again.");
                if !console {
                    typed = Some(password_typed(options, &tool)?);
                }
            }
            Answer::Refused(why) => return Err(fail(why)),
        }
    }
}

/// Whether the console may ask for the password (ADR 0251): unless -n
/// says not to ask or -S to read it from the standard input. On a pane
/// that is not a console's, authd answers that elevate is to ask.
fn console_asks(options: &Options) -> bool {
    !options.non_interactive && !options.stdin
}

/// The name this program was run by, for its prompt.
fn tool_name() -> String {
    let name = std::env::args_os().next().unwrap_or_default();
    let name = std::path::Path::new(&name).file_name().map(|n| n.to_string_lossy().into_owned());
    match name.as_deref() {
        Some("sudo") => "sudo".into(),
        _ => "elevate".into(),
    }
}

/// The write end of the pipe the signal handler wakes the loop with.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let byte = signal as u8;
    // SAFETY: write(2) is async-signal-safe; a full pipe only loses a
    // repeat of a signal.
    unsafe { libc::write(WAKE.load(Ordering::Relaxed), (&raw const byte).cast(), 1) };
}

/// The signals elevate passes on to the command, as sudo does, and the
/// one that tells it its terminal's size changed.
const PASSED_ON: [libc::c_int; 5] =
    [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP, libc::SIGTSTP];

/// A pipe, its ends closed on exec: (read, write).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 writes two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both are new descriptors of ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The command's own terminal: a pseudo-terminal with the settings and
/// size of `like`, one of elevate's.
struct Pty {
    master: OwnedFd,
    slave: Option<OwnedFd>,
}

impl Pty {
    fn open(like: RawFd) -> io::Result<Pty> {
        // SAFETY: all zeros is a valid termios and winsize: numbers.
        let (mut settings, mut size): (libc::termios, libc::winsize) =
            unsafe { (util::zeroed(), util::zeroed()) };
        // SAFETY: tcgetattr and TIOCGWINSZ fill what they are given;
        // failing leaves zeros, which openpty is not given.
        let have_settings = unsafe { libc::tcgetattr(like, &raw mut settings) } == 0;
        // SAFETY: as above.
        let have_size = unsafe { libc::ioctl(like, libc::TIOCGWINSZ, &raw mut size) } == 0;
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty writes the two descriptors; the name may be
        // null, and so may the settings and size.
        let opened = unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                std::ptr::null_mut(),
                if have_settings { &raw const settings } else { std::ptr::null() },
                if have_size { &raw const size } else { std::ptr::null() },
            )
        };
        if opened != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty gave both descriptors to us.
        let pty = unsafe {
            Pty { master: OwnedFd::from_raw_fd(master), slave: Some(OwnedFd::from_raw_fd(slave)) }
        };
        for fd in [master, slave] {
            // SAFETY: setting close-on-exec on our own descriptor.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        Ok(pty)
    }
}

/// The terminal's settings from before it was made raw, put back when it
/// is dropped, whatever ends elevate's copying.
struct Raw {
    fd: RawFd,
    saved: Option<libc::termios>,
}

impl Raw {
    fn new(fd: RawFd) -> Raw {
        Raw { fd, saved: None }
    }

    fn on(&mut self) {
        // SAFETY: all zeros is a valid termios: numbers.
        let mut settings: libc::termios = unsafe { util::zeroed() };
        // SAFETY: tcgetattr fills `settings`.
        if unsafe { libc::tcgetattr(self.fd, &raw mut settings) } != 0 {
            return;
        }
        if self.saved.is_none() {
            self.saved = Some(settings);
        }
        let mut raw = settings;
        // SAFETY: cfmakeraw changes the termios it is given; `raw` is
        // valid for tcsetattr.
        unsafe {
            libc::cfmakeraw(&raw mut raw);
            libc::tcsetattr(self.fd, libc::TCSADRAIN, &raw const raw);
        }
    }

    fn off(&mut self) {
        if let Some(saved) = self.saved.take() {
            // SAFETY: `saved` is the termios read before.
            unsafe { libc::tcsetattr(self.fd, libc::TCSADRAIN, &raw const saved) };
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        self.off();
    }
}

/// Writes all of `data` to `fd`, as far as it takes it.
fn write_all(fd: RawFd, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: writes from our buffer.
        let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if n <= 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        data = &data[n as usize..];
    }
}

/// Reads what `fd` has into `buffer`: the count, 0 at its end or on an
/// error other than an interrupted read.
fn read_some(fd: RawFd, buffer: &mut [u8]) -> usize {
    loop {
        // SAFETY: reads into our buffer.
        let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if n >= 0 {
            return n as usize;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return 0;
        }
    }
}

/// The exit status a command's wait status `status` makes elevate's, as
/// a shell gives it: its exit code, or 128 and the signal that ended it.
fn exit_code(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// Runs the command as an administrator: asks authd, with the password
/// if it wants one, and copies between the terminal and the command's
/// until it ends.
fn run(options: &Options) -> Result<i32, Fail> {
    let tool = tool_name();
    let is_tty = |fd: RawFd| util::is_terminal(fd);
    let ttys = [is_tty(0), is_tty(1), is_tty(2)];
    let mut pty = match (0..3).find(|&fd| ttys[fd as usize]) {
        Some(like) => Some(Pty::open(like).map_err(|e| util::failed(&"a pseudo-terminal", e))?),
        None => None,
    };
    let slave = pty.as_ref().and_then(|p| p.slave.as_ref()).map(AsRawFd::as_raw_fd);
    let mut fds: Vec<RawFd> =
        (0..3).map(|fd| if ttys[fd as usize] { slave.unwrap_or(fd) } else { fd }).collect();
    let (relay_read, relay_write) = pipe().map_err(|e| util::failed(&"a pipe", e))?;
    fds.push(relay_read.as_raw_fd());
    let here =
        std::env::current_dir().map_or_else(|_| b"/".to_vec(), |d| d.into_os_string().into_vec());
    let term = std::env::var("TERM").unwrap_or_default();
    let language = own_language();
    let mut typed: Option<Vec<u8>> = None;
    let mut console = console_asks(options);
    let mut wrong = 0;
    let (process, mut reader) = loop {
        let password = typed.take();
        let asks = console && password.is_none();
        let request = Request::Elevate {
            mode: options.mode,
            password,
            ask: asks,
            term: term.clone(),
            directory: here.clone(),
            language: language.clone(),
            command: options.command.clone(),
        };
        match ask_with(&request, &fds)? {
            // Not a console's pane, or no consoled: elevate asks.
            (Answer::Done(text), _) if text == NEEDS_PASSWORD => {
                console = false;
                typed = Some(password_typed(options, &tool)?);
            }
            (Answer::Done(text), reader) => match text.parse::<u64>() {
                Ok(process) => break (process, reader),
                Err(_) => return Err(fail(format!("authd answered `ok {text}`"))),
            },
            (Answer::Refused(why), _) if why == WRONG_PASSWORD => {
                wrong += 1;
                if wrong >= TRIES {
                    return Err(fail(format!("{TRIES} wrong passwords")));
                }
                eprintln!("Sorry, try again.");
                if !console {
                    typed = Some(password_typed(options, &tool)?);
                }
            }
            (Answer::Refused(why), _) => return Err(fail(why)),
        }
    };
    let _ = process;
    // authd has the command's ends now.
    if let Some(pty) = &mut pty {
        pty.slave = None;
    }
    drop(relay_read);
    let relay = relay_write.as_raw_fd();
    let (wake_read, wake_write) = pipe().map_err(|e| util::failed(&"a pipe", e))?;
    WAKE.store(wake_write.as_raw_fd(), Ordering::Relaxed);
    // SAFETY: the handler only writes to the pipe; the signals' old
    // actions are not needed again, as elevate ends after.
    unsafe {
        let mut action: libc::sigaction = util::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        for signal in PASSED_ON.into_iter().chain([libc::SIGWINCH]) {
            libc::sigaction(signal, &raw const action, std::ptr::null_mut());
        }
        // A closed output must not end elevate before it puts the
        // terminal back.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // Keys go to the command's terminal from elevate's own, raw, so that
    // Ctrl+C and Ctrl+Z are the command's terminal's to act on.
    let input = pty.is_some() && ttys[0];
    let mut raw = Raw::new(0);
    if input {
        raw.on();
    }
    // What the command writes to its terminal shows on elevate's.
    let out = [1, 2, 0].into_iter().find(|&fd| ttys[fd as usize]).unwrap_or(1);
    let master = pty.as_ref().map(|p| p.master.as_raw_fd());
    let mut master_open = master.is_some();
    let mut input_open = input;
    let mut buffer = [0u8; 4096];
    let mut line = String::new();
    loop {
        let mut fds = [
            libc::pollfd { fd: reader.get_ref().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd {
                fd: if master_open { master.unwrap_or(-1) } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd { fd: if input_open { 0 } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        // A line of authd's already read is taken before waiting.
        let buffered = !reader.buffer().is_empty();
        // SAFETY: `fds` holds valid pollfds.
        let ready = unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, if buffered { 0 } else { -1 })
        };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(fail(format!("poll: {}", io::Error::last_os_error())));
        }
        if fds[1].revents != 0 {
            let mut signals = [0u8; 16];
            let n = read_some(wake_read.as_raw_fd(), &mut signals);
            for &signal in &signals[..n] {
                let signal = libc::c_int::from(signal);
                if signal == libc::SIGWINCH {
                    if let Some(master) = master {
                        // SAFETY: copies the terminal's size to the
                        // command's.
                        unsafe {
                            let mut size: libc::winsize = util::zeroed();
                            if libc::ioctl(out, libc::TIOCGWINSZ, &raw mut size) == 0 {
                                libc::ioctl(master, libc::TIOCSWINSZ, &raw const size);
                            }
                        }
                    }
                } else {
                    write_all(relay, request::signal(signal).as_bytes());
                }
            }
        }
        if fds[3].revents != 0 {
            let n = read_some(0, &mut buffer);
            if n == 0 {
                input_open = false;
            } else if let Some(master) = master {
                write_all(master, &buffer[..n]);
            }
        }
        if fds[2].revents != 0
            && let Some(master) = master
        {
            let n = read_some(master, &mut buffer);
            if n == 0 {
                // Every descriptor of the command's terminal is closed.
                master_open = false;
            } else {
                write_all(out, &buffer[..n]);
            }
        }
        if fds[0].revents != 0 || buffered {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    raw.off();
                    return Err(fail("authd went away before the command ended".into()));
                }
                Ok(_) => {}
            }
            if let Some(status) = request::ended_status(&line) {
                // What the command wrote last is still to be shown.
                if let Some(master) = master.filter(|_| master_open) {
                    drain(master, out);
                }
                raw.off();
                return Ok(exit_code(status));
            }
            if request::stopped_signal(&line).is_some() {
                // The command stopped, as with Ctrl+Z: elevate stops too,
                // so that the shell that ran it has its terminal back, and
                // has the command go on when it does.
                raw.off();
                // SAFETY: stopping ourselves; we go on after SIGCONT.
                unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
                if input {
                    raw.on();
                }
                write_all(relay, request::CONTINUE.as_bytes());
            }
        }
    }
}

/// Copies what the command's terminal still holds to `out`, without
/// waiting for more.
fn drain(master: RawFd, out: RawFd) {
    let mut buffer = [0u8; 4096];
    loop {
        let mut entry = libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&raw mut entry, 1, 0) } <= 0 {
            return;
        }
        let n = read_some(master, &mut buffer);
        if n == 0 {
            return;
        }
        write_all(out, &buffer[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> User {
        root_account(std::iter::empty())
    }

    fn words(text: &[&str]) -> Vec<Vec<u8>> {
        text.iter().map(|w| w.as_bytes().to_vec()).collect()
    }

    #[test]
    fn only_language_settings_pass() {
        for kept in ["LANG=C.UTF-8", "LC_ALL=en_US.UTF-8", "LANGUAGE=de:en", "LC_TIME=de_DE@euro"] {
            assert!(language_setting(kept), "{kept}");
        }
        for dropped in [
            "PATH=/users/alice/bin",
            "LD_PRELOAD=/tmp/x.so",
            "LC_ALL=../../users/alice/evil",
            "LANG=a b",
            "LC_=x",
            "LC_all=x",
            "LANG",
        ] {
            assert!(!language_setting(dropped), "{dropped}");
        }
    }

    #[test]
    fn the_environment_is_root_s_with_the_fixed_path() {
        let alice = accounts::users("alice:x:1000:1000::/users/alice:/system/commands/nukesh\n")
            .entries()
            .next()
            .cloned()
            .unwrap();
        let language = vec!["LANG=C.UTF-8".to_string(), "EVIL=1".to_string()];
        let env = environment(&root(), &alice, "linux", &language, "/system/commands/dmesg");
        for set in [
            "HOME=/",
            "USER=root",
            "LOGNAME=root",
            "PATH=/system/commands:/applications/commands",
            "TERM=linux",
            "SUDO_USER=alice",
            "SUDO_UID=1000",
            "SUDO_GID=1000",
            "SUDO_COMMAND=/system/commands/dmesg",
            "LANG=C.UTF-8",
        ] {
            assert!(env.iter().any(|e| e == set), "{set}");
        }
        assert!(!env.iter().any(|e| e.starts_with("EVIL")));
        assert!(environment(&root(), &alice, "x\ny", &[], "").iter().any(|e| e == "TERM=dumb"));
    }

    #[test]
    fn shells_get_one_escaped_line() {
        assert_eq!(shell_line(&words(&["ls", "-l", "/users/bob"])), b"ls -l /users/bob");
        assert_eq!(shell_line(&words(&["echo", "a b;c", "$HOME"])), b"echo a\\ b\\;c $HOME");
        assert_eq!(shell_line(&words(&["x", "it's"])), b"x it\\'s");
    }

    #[test]
    fn shell_plans() {
        let shell = plan(Mode::Shell, &[], b"/users/alice", &root()).unwrap();
        assert_eq!(shell.program, b"/system/commands/nukesh");
        assert_eq!(shell.argv, words(&["nukesh"]));
        assert_eq!(shell.directory, b"/users/alice");
        let login = plan(Mode::Login, &words(&["id", "-u"]), b"/users/alice", &root()).unwrap();
        assert_eq!(login.argv, words(&["-nukesh", "-c", "id -u"]));
        assert_eq!(login.directory, b"/");
        assert_eq!(login.shown, "/system/commands/nukesh -c id -u");
    }

    #[test]
    fn commands_are_found_only_where_they_may_be() {
        let missing = plan(Mode::Command, &words(&["no-such-command-here"]), b"/", &root());
        assert_eq!(missing, Err("no-such-command-here: command not found".into()));
        assert_eq!(resolve(b"", b"/"), None);
        // A path is taken as given: a directory is no command.
        assert_eq!(resolve(b"/", b"/"), None);
        assert_eq!(resolve(b"./", b"/tmp"), None);
    }

    #[test]
    fn command_lines_show_on_one_line() {
        assert_eq!(shown_command(&words(&["/x", "a\nb", "\u{1b}[2J"])), "/x a?b ?[2J");
        assert_eq!(shown_command(&[vec![b'a'; 1000]]).len(), SHOWN_MAX);
    }

    #[test]
    fn exit_codes_are_a_shell_s() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(3 << 8), 3);
        assert_eq!(exit_code(libc::SIGINT), 128 + libc::SIGINT);
    }
}
