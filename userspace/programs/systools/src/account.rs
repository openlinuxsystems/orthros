// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! user(8) and passwd(1): accounts and their passwords, through authd(8)
//! (ADR 0218), which alone changes the account files. user(8) also
//! disables, enables, removes, renames and changes accounts (ADR 0220),
//! and keeps, archives or deletes a removed account's home itself.
//!
//! A password is typed at the terminal, unseen, and again to be sure. No
//! rules are forced on it: an empty or short one gets a plain warning
//! and a question, and the person who answers decides. With no terminal,
//! as in a script, each comes as a line of the standard input, with
//! neither the second time nor the question.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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

/// user(8): makes, lists, disables, enables, removes, renames and
/// changes accounts.
pub fn user(args: &mut Args) -> Result<i32, Fail> {
    let help = "Usage: user add [--admin] [--full-name NAME] name\n       \
                user list\n       \
                user disable [--yes] name\n       \
                user enable name\n       \
                user remove [--keep-home | --archive-home | --delete-home] [--yes] name\n       \
                user rename name new-name\n       \
                user set [--full-name NAME] [--admin | --no-admin] [--yes] name";
    let Some(subcommand) = args.subcommand() else {
        return match args.next()? {
            Some(Arg::Long(l)) if l == "help" => {
                println!("{help}");
                Ok(0)
            }
            Some(other) => Err(args.unknown(&other)),
            None => Err(usage(format!(
                "which? add, list, disable, enable, remove, rename or set\n{help}"
            ))),
        };
    };
    match subcommand.to_str() {
        Some("add") => add(args),
        Some("list") => list(args),
        Some("disable") => disable(args),
        Some("enable") => enable(args),
        Some("remove") => remove(args),
        Some("rename") => rename(args),
        Some("set") => set(args),
        _ => Err(usage(format!("no such command: {}\n{help}", subcommand.to_string_lossy()))),
    }
}

/// The names given after a subcommand's options, `count` of them, and
/// whether `--yes` was: the options a subcommand takes besides are
/// `take`n first.
fn operands(
    args: &mut Args,
    what: &str,
    count: usize,
    mut take: impl FnMut(&mut Args, &Arg) -> Result<bool, Fail>,
) -> Result<(Vec<String>, bool), Fail> {
    let (mut names, mut yes) = (Vec::new(), false);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(ref l) if l == "yes" => yes = true,
            Arg::Short('y') => yes = true,
            Arg::Operand(ref operand) if names.len() < count => {
                let name =
                    operand.to_str().ok_or_else(|| fail("a name is letters and digits".into()))?;
                names.push(name.to_string());
            }
            other => {
                if !take(args, &other)? {
                    return Err(args.unknown(&other));
                }
            }
        }
    }
    if names.len() < count {
        return Err(usage(what.to_string()));
    }
    Ok((names, yes))
}

/// The account files as they are, and whether each person's account is
/// disabled, as only root reads it: `None` for the others.
struct Accounts {
    users: accounts::Table<User>,
    groups: accounts::Table<accounts::Group>,
    disabled: Option<Vec<String>>,
}

impl Accounts {
    fn read() -> Result<Accounts, Fail> {
        let read = |path: &str| std::fs::read_to_string(path).map_err(|e| util::failed(&path, e));
        let disabled = std::fs::read_to_string(accounts::PASSWORDS).ok().map(|text| {
            accounts::passwords(&text)
                .entries()
                .filter(|p| p.is_disabled())
                .map(|p| p.name.clone())
                .collect()
        });
        Ok(Accounts {
            users: accounts::users(&read(accounts::PASSWD)?),
            groups: accounts::groups(&read(accounts::GROUP)?),
            disabled,
        })
    }

    fn user(&self, name: &str) -> Result<&User, Fail> {
        self.users
            .entries()
            .find(|u| u.name == name)
            .ok_or_else(|| fail(format!("there is no account {name}")))
    }

    fn is_disabled(&self, name: &str) -> bool {
        self.disabled.as_ref().is_some_and(|names| names.iter().any(|n| n == name))
    }

    /// The administrators who can sign in, besides `name`.
    fn other_admins(&self, name: &str) -> usize {
        let admin = |u: &&User| u.is_person() && accounts::is_admin(u, &self.groups);
        self.users
            .entries()
            .filter(admin)
            .filter(|u| u.name != name && !self.is_disabled(&u.name))
            .count()
    }
}

/// Refuses to act on the account of the administrator who runs this,
/// which elevate names (`SUDO_USER`): the session it runs in would end
/// under it. Another administrator does it, as on other systems.
fn not_own(name: &str, what: &str) -> Result<(), Fail> {
    if std::env::var("SUDO_USER").is_ok_and(|asker| asker == name) {
        return Err(fail(format!(
            "{name} is the account you are signed in with: another administrator {what} it"
        )));
    }
    Ok(())
}

/// Goes on only if `name`, an administrator, is not the last who can
/// sign in, or the person says so, warned: with none, no one can
/// elevate.
fn last_admin(accounts: &Accounts, name: &str, yes: bool, gone: &str) -> Result<(), Fail> {
    let user = accounts.user(name)?;
    if !accounts::is_admin(user, &accounts.groups) || accounts.other_admins(name) > 0 {
        return Ok(());
    }
    eprintln!(
        "{name} is the last administrator who can sign in. {gone}, no one can elevate to run \
         commands as an administrator, or make another."
    );
    if yes || (prompt::interactive() && prompt::confirm("Go on anyway?")) {
        Ok(())
    } else if prompt::interactive() {
        Err(fail("nothing changed".into()))
    } else {
        Err(fail("nothing changed: say --yes to go on".into()))
    }
}

/// `user disable`: nothing signs in to the account, and everything it
/// runs ends at once.
fn disable(args: &mut Args) -> Result<i32, Fail> {
    let (names, yes) = operands(args, "user disable needs a name", 1, |_, _| Ok(false))?;
    let name = &names[0];
    let accounts = Accounts::read()?;
    accounts.user(name)?;
    not_own(name, "disables")?;
    last_admin(
        &accounts,
        name,
        yes,
        "With it disabled, until the startup-trouble menu resets its password",
    )?;
    let answer = ask(&Request::Disable { name: name.clone() })?;
    done(answer, |ended| {
        let ended = match ended {
            "0" => "nothing of it was running".to_string(),
            "1" => "1 of its processes ended".to_string(),
            n => format!("{n} of its processes ended"),
        };
        println!("{name} is disabled: {ended}. user enable {name} enables it again.");
    })
}

/// `user enable`: the account signs in again, with the password it had.
fn enable(args: &mut Args) -> Result<i32, Fail> {
    let (names, _) = operands(args, "user enable needs a name", 1, |_, _| Ok(false))?;
    let name = &names[0];
    let answer = ask(&Request::Enable { name: name.clone() })?;
    done(answer, |said| {
        if said.is_empty() {
            println!("{name} is enabled again.");
        } else {
            println!("{said}.");
        }
    })
}

/// What becomes of a removed account's home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    /// It stays where it is, its files still the removed account's
    /// number, which no account gets again.
    Keep,
    /// It goes into one compressed file in `/users/.removed`, for
    /// administrators only.
    Archive,
    /// It goes.
    Delete,
}

/// The answer to which [`Home`] at the terminal.
fn ask_home(home: &str) -> Result<Home, Fail> {
    loop {
        let question = format!("Keep, archive or delete its home, {home}? [keep] ");
        match prompt::ask(&question, true) {
            Ok(prompt::Answer::Line(line)) => match line.trim_ascii() {
                b"" | b"k" | b"keep" => return Ok(Home::Keep),
                b"a" | b"archive" => return Ok(Home::Archive),
                b"d" | b"delete" => return Ok(Home::Delete),
                _ => eprintln!("Type keep, archive or delete."),
            },
            Ok(prompt::Answer::Stopped) => return Err(fail("stopped; nothing changed".into())),
            Err(err) => return Err(util::failed(&"the terminal", err)),
        }
    }
}

/// `user remove`: the account goes, everything it runs ends, and its
/// home is kept, archived or deleted, as the person says.
fn remove(args: &mut Args) -> Result<i32, Fail> {
    let mut home: Option<Home> = None;
    let (names, yes) = operands(args, "user remove needs a name", 1, |_, arg| {
        let chosen = match arg {
            Arg::Long(l) if l == "keep-home" => Home::Keep,
            Arg::Long(l) if l == "archive-home" => Home::Archive,
            Arg::Long(l) if l == "delete-home" => Home::Delete,
            _ => return Ok(false),
        };
        if home.is_some_and(|h| h != chosen) {
            return Err(usage("say one of --keep-home, --archive-home and --delete-home".into()));
        }
        home = Some(chosen);
        Ok(true)
    })?;
    let name = &names[0];
    let accounts = Accounts::read()?;
    let user = accounts.user(name)?.clone();
    not_own(name, "removes")?;
    if !user.is_person() {
        return Err(fail(format!("{name} is not a person's account")));
    }
    // Only a home of its own in /users is archived or deleted.
    let own_home = user.home == format!("{}/{name}", accounts::USERS);
    let home = match home {
        Some(home) => home,
        None if !own_home => Home::Keep,
        None if prompt::interactive() => ask_home(&user.home)?,
        None => {
            return Err(usage(
                "say what becomes of its home: --keep-home, --archive-home or --delete-home".into(),
            ));
        }
    };
    if home != Home::Keep && !own_home {
        return Err(fail(format!(
            "{name}'s home is {}, not {}/{name}: it is kept, for you to see to",
            user.home,
            accounts::USERS
        )));
    }
    last_admin(&accounts, name, yes, "With it removed")?;
    if home == Home::Delete && !yes && prompt::interactive() {
        let question = format!("Delete {} and every file in it, for good?", user.home);
        if !prompt::confirm(&question) {
            return Err(fail("nothing changed".into()));
        }
    }
    // The terminal may close under it: what is left to do is done all
    // the same.
    // SAFETY: ignoring SIGHUP and SIGPIPE changes no memory.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let answer = ask(&Request::Remove { name: name.clone() })?;
    let Answer::Done(home_path) = answer else {
        return done(answer, |_| {});
    };
    let said = match home {
        Home::Keep => format!(
            "its home, {home_path}, is kept, its files still number {}'s, which no account gets \
             again",
            user.uid
        ),
        Home::Archive => {
            let archive = archive_home(&user)?;
            delete_home(&user)?;
            format!("its home is archived in {archive}, for administrators only")
        }
        Home::Delete => {
            delete_home(&user)?;
            format!("its home, {home_path}, is deleted")
        }
    };
    let _ = writeln!(io::stdout(), "{name} is removed: {said}.");
    Ok(0)
}

/// Packs `user`'s home into a new file in [`accounts::REMOVED`], with
/// tar(1) and zstd; the file's path.
fn archive_home(user: &User) -> Result<String, Fail> {
    let removed = std::path::Path::new(accounts::REMOVED);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(removed)
        .or_else(|err| if removed.is_dir() { Ok(()) } else { Err(err) })
        .map_err(|e| util::failed(&accounts::REMOVED, e))?;
    let day = util::local_time(util::now_ns() as i64 / 1_000_000_000, "%Y-%m-%d");
    let path = (1..)
        .map(|n| {
            let tail = if n == 1 { String::new() } else { format!("-{n}") };
            format!("{}/{}-{day}{tail}.tar.zst", accounts::REMOVED, user.name)
        })
        .find(|path| !std::path::Path::new(path).exists())
        .unwrap_or_default();
    let status = std::process::Command::new("/system/commands/tar")
        .args(["--zstd", "-cf", &path, "-C", accounts::USERS, &user.name])
        .status()
        .map_err(|e| util::failed(&"/system/commands/tar", e))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(fail(format!("tar could not archive {}: its home is kept", user.home)));
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| util::failed(&path, e))?;
    Ok(path)
}

/// Deletes `user`'s home, its own in /users.
fn delete_home(user: &User) -> Result<(), Fail> {
    match std::fs::remove_dir_all(&user.home) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(util::failed(&user.home, err)),
        _ => Ok(()),
    }
}

/// `user rename`: the account gets a new name, with its own group and
/// its home, once it runs nothing.
fn rename(args: &mut Args) -> Result<i32, Fail> {
    let (names, _) =
        operands(args, "user rename needs the name and the new name", 2, |_, _| Ok(false))?;
    let (name, new) = (&names[0], &names[1]);
    accounts::check_name(new).map_err(fail)?;
    not_own(name, "renames")?;
    let answer = ask(&Request::Rename { name: name.clone(), new: new.clone() })?;
    done(answer, |text| println!("{}.", capitalized(text)))
}

/// `user set`: changes an account's full name, or whether it is an
/// administrator's.
fn set(args: &mut Args) -> Result<i32, Fail> {
    let (mut full_name, mut admin) = (None, None);
    let (names, yes) = operands(args, "user set needs a name", 1, |args, arg| {
        match arg {
            Arg::Long(l) if l == "full-name" => full_name = Some(args.text()?),
            Arg::Long(l) if l == "admin" => admin = Some(true),
            Arg::Long(l) if l == "no-admin" => admin = Some(false),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let name = &names[0];
    if full_name.is_none() && admin.is_none() {
        return Err(usage("say what to change: --full-name, --admin or --no-admin".into()));
    }
    if let Some(full_name) = &full_name {
        accounts::check_full_name(full_name).map_err(fail)?;
    }
    if admin == Some(false) {
        let accounts = Accounts::read()?;
        last_admin(&accounts, name, yes, "Without it")?;
    }
    let answer = ask(&Request::Set { name: name.clone(), full_name, admin })?;
    done(answer, |changed| println!("{name} is changed: {changed}."))
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
    let accounts = Accounts::read()?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "{:<16} {:>6}  {:<13}  {:<8}  {:<20} FULL NAME",
        "NAME", "NUMBER", "KIND", "STATE", "HOME"
    )?;
    for user in accounts.users.entries().filter(|u| u.is_person()) {
        let kind =
            if accounts::is_admin(user, &accounts.groups) { "administrator" } else { "user" };
        // Whether it is disabled is root's to read.
        let state = match &accounts.disabled {
            None => "-",
            Some(_) if accounts.is_disabled(&user.name) => "disabled",
            Some(_) => "enabled",
        };
        let line = format!(
            "{:<16} {:>6}  {kind:<13}  {state:<8}  {:<20} {}",
            user.name, user.uid, user.home, user.full_name
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    Ok(0)
}
