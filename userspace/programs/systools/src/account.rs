// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! user(8) and passwd(1): accounts and their passwords, through authd(8)
//! (ADR 0218), which alone changes the account files.
//!
//! A password is typed at the terminal, unseen, and again to be sure. No
//! rules are forced on it: an empty or short one gets a plain warning
//! and a question, and the person who answers decides. With no terminal,
//! as in a script, each comes as a line of the standard input, with
//! neither the second time nor the question.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use accounts::request::{Answer, Request};
use accounts::{User, wipe};

use crate::args::{Arg, Args};
use crate::prompt;
use crate::util::{self, Fail, fail, usage};

/// Asks authd `request`; its answer.
pub fn ask(request: &Request) -> Result<Answer, Fail> {
    let unreachable = |err: io::Error| {
        fail(format!("cannot reach authd at {}: {}", accounts::SOCKET, util::strip_os_error(&err)))
    };
    let mut stream = UnixStream::connect(accounts::SOCKET).map_err(unreachable)?;
    let mut bytes = request.encode();
    let sent = stream.write_all(&bytes).and_then(|()| stream.shutdown(Shutdown::Write));
    wipe(&mut bytes);
    sent.map_err(unreachable)?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).map_err(unreachable)?;
    Ok(Answer::decode(&answer))
}

/// The account `name`, from the passwd file.
fn account(name: &str) -> Result<User, Fail> {
    let text = std::fs::read_to_string(accounts::PASSWD)
        .map_err(|e| util::failed(&accounts::PASSWD, e))?;
    accounts::users(&text)
        .entries()
        .find(|u| u.name == name)
        .cloned()
        .ok_or_else(|| fail(format!("there is no account {name}")))
}

/// The name of the account the program runs for.
fn own_name() -> Result<String, Fail> {
    let uid = util::own_user();
    let text = std::fs::read_to_string(accounts::PASSWD)
        .map_err(|e| util::failed(&accounts::PASSWD, e))?;
    accounts::users(&text)
        .entries()
        .find(|u| u.uid == uid)
        .map(|u| u.name.clone())
        .ok_or_else(|| fail(format!("user {uid} has no account")))
}

/// Where passwords come from: the terminal, or lines of the standard
/// input.
pub(crate) enum Input {
    Terminal,
    Lines(io::StdinLock<'static>),
}

impl Input {
    pub(crate) fn new() -> Input {
        if prompt::interactive() { Input::Terminal } else { Input::Lines(io::stdin().lock()) }
    }

    /// A password, asked with `prompt`.
    pub(crate) fn password(&mut self, prompt: &str) -> Result<Vec<u8>, Fail> {
        match self {
            Input::Terminal => match prompt::ask(prompt, false) {
                Ok(prompt::Answer::Line(line)) => Ok(line),
                Ok(prompt::Answer::Stopped) => Err(fail("stopped; nothing changed".into())),
                Err(err) => Err(util::failed(&"the terminal", err)),
            },
            Input::Lines(lines) => {
                let mut line = Vec::new();
                match lines.read_until(b'\n', &mut line) {
                    Ok(0) => Err(fail("the standard input ended before a password".into())),
                    Ok(_) => {
                        if line.ends_with(b"\n") {
                            line.pop();
                        }
                        Ok(line)
                    }
                    Err(err) => Err(util::failed(&"standard input", err)),
                }
            }
        }
    }

    /// A new password for `name`: asked twice at a terminal, which must
    /// agree, then warned about if it is empty or short, and kept only if
    /// the person says so.
    pub(crate) fn new_password(&mut self, name: &str) -> Result<Vec<u8>, Fail> {
        let mut password = self.password("New password: ")?;
        if let Input::Terminal = self {
            let mut again = self.password("The same again: ")?;
            let same = again == password;
            wipe(&mut again);
            if !same {
                wipe(&mut password);
                return Err(fail("the two were not the same; nothing changed".into()));
            }
        }
        let text = String::from_utf8_lossy(&password).into_owned();
        if let Some(warning) = accounts::password_warning(name, &text) {
            eprintln!("{warning}");
            if let Input::Terminal = self
                && !prompt::confirm("Use it anyway?")
            {
                wipe(&mut password);
                return Err(fail("nothing changed".into()));
            }
        }
        Ok(password)
    }
}

/// What authd's answer comes to: done, saying what it said, or failed.
fn done(answer: Answer, say: impl FnOnce(&str)) -> Result<i32, Fail> {
    match answer {
        Answer::Done(text) => {
            say(&text);
            Ok(0)
        }
        Answer::Refused(why) => Err(fail(why)),
    }
}

/// passwd(1): changes your password, or as root, anyone's.
pub fn passwd(args: &mut Args) -> Result<i32, Fail> {
    let mut name = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Operand(operand) if name.is_none() => {
                name = Some(operand.to_string_lossy().into_owned());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    let name = match name {
        Some(name) => name,
        None => own_name()?,
    };
    account(&name)?;
    let mut input = Input::new();
    if let Input::Terminal = input {
        eprintln!("Changing the password of {name}.");
    }
    // Root changes anyone's without theirs (from 13b.6, an
    // administrator inside elevate).
    let current =
        if util::own_user() == 0 { Vec::new() } else { input.password("Current password: ")? };
    let new = input.new_password(&name)?;
    let answer = ask(&Request::Passwd { name: name.clone(), current, new })?;
    done(answer, |_| eprintln!("The password of {name} is changed."))
}

/// user(8): makes and lists accounts.
pub fn user(args: &mut Args) -> Result<i32, Fail> {
    let help = "Usage: user add [--admin] [--full-name NAME] name\n       user list";
    let Some(subcommand) = args.subcommand() else {
        return match args.next()? {
            Some(Arg::Long(l)) if l == "help" => {
                println!("{help}");
                Ok(0)
            }
            Some(other) => Err(args.unknown(&other)),
            None => Err(usage(format!("which? add or list\n{help}"))),
        };
    };
    match subcommand.to_str() {
        Some("add") => add(args),
        Some("list") => list(args),
        _ => Err(usage(format!("no such command: {}\n{help}", subcommand.to_string_lossy()))),
    }
}

/// `user add`: makes an account, with its group, its password and its
/// home.
fn add(args: &mut Args) -> Result<i32, Fail> {
    let (mut admin, mut full_name, mut name) = (false, String::new(), None::<OsString>);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(l) if l == "admin" => admin = true,
            Arg::Long(l) if l == "full-name" => full_name = args.text()?,
            Arg::Operand(operand) if name.is_none() => name = Some(operand),
            other => return Err(args.unknown(&other)),
        }
    }
    let name = name.ok_or_else(|| usage("user add needs a name".into()))?;
    let name = name.into_string().map_err(|_| fail("a name is letters and digits".into()))?;
    accounts::check_name(&name).map_err(fail)?;
    accounts::check_full_name(&full_name).map_err(fail)?;
    let mut input = Input::new();
    if let Input::Terminal = input {
        let kind = if admin { "an administrator's account" } else { "an account" };
        eprintln!("Making {kind} for {name}.");
    }
    let password = input.new_password(&name)?;
    let answer = ask(&Request::Add { name, full_name, admin, password })?;
    done(answer, |text| println!("{}", capitalized(text)))
}

/// `text` with a capital first letter, as a sentence starts.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// `user list`: the people's accounts, as the passwd and group files
/// have them.
fn list(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    let read = |path: &str| std::fs::read_to_string(path).map_err(|e| util::failed(&path, e));
    let users = accounts::users(&read(accounts::PASSWD)?);
    let groups = accounts::groups(&read(accounts::GROUP)?);
    let mut out = io::stdout().lock();
    writeln!(out, "{:<16} {:>6}  {:<13}  {:<20} FULL NAME", "NAME", "NUMBER", "KIND", "HOME")?;
    for user in users.entries().filter(|u| u.is_person()) {
        let kind = if accounts::is_admin(user, &groups) { "administrator" } else { "user" };
        let line = format!(
            "{:<16} {:>6}  {kind:<13}  {:<20} {}",
            user.name, user.uid, user.home, user.full_name
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(0)
}
