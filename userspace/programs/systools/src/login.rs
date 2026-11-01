// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! login(1): signing in at a console (ADR 0250).
//!
//! consoled(8) runs `login` as root on each console's terminal. It asks
//! for a name and a password and sends them to authd(8) with the
//! terminal; authd checks them and starts the person's session there,
//! acting for them, and tells login when it ends. login then ends too,
//! and consoled shows the sign-in again. On a machine no account can be
//! signed in to yet, login first makes its owner's, an administrator's.
//!
//! In a session, authd runs `login --session` first, as the person: it
//! makes the terminal the session's controlling terminal, goes to their
//! home and runs their shell as a login shell, as login(1) does on Linux
//! after its checks.
//!
//! A computer started from its boot menu's startup-trouble entry, with
//! the kernel flag `startup-trouble`, shows the startup-trouble menu
//! before the sign-in (ADR 0253): it resets an administrator's password
//! through authd, which tells every administrator at their next sign-in,
//! or restarts or powers off the computer.

use std::ffi::CString;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use accounts::request::{Answer, Request};
use accounts::wipe;

use crate::account::Input;
use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail};
use crate::{authd, prompt, signin};

/// How long login waits for authd to answer at all, as at boot, when
/// the consoles may come up before it.
const AUTHD_WAIT: Duration = Duration::from_secs(30);

/// Connects to authd, waiting for it for up to [`AUTHD_WAIT`].
fn connect() -> Result<UnixStream, Fail> {
    let start = Instant::now();
    let mut told = false;
    loop {
        match UnixStream::connect(accounts::SOCKET) {
            Ok(stream) => return Ok(stream),
            Err(err) if start.elapsed() >= AUTHD_WAIT => {
                return Err(fail(format!(
                    "cannot reach authd at {}: {}",
                    accounts::SOCKET,
                    util::strip_os_error(&err)
                )));
            }
            Err(_) => {
                if !told {
                    eprintln!("Waiting for authd...");
                    told = true;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Asks authd `request`, sending the terminal in descriptor 0 with it if
/// `terminal`; the connection, its request sent, to read the answer from.
fn send(request: &Request, terminal: bool) -> Result<UnixStream, Fail> {
    let stream = connect()?;
    let mut bytes = request.encode();
    let sent = if terminal {
        signin::send_with_fds(&stream, &bytes, &[0])
    } else {
        (&stream).write_all(&bytes)
    };
    wipe(&mut bytes);
    sent.and_then(|()| stream.shutdown(Shutdown::Write))
        .map_err(|err| fail(format!("cannot ask authd: {}", util::strip_os_error(&err))))?;
    Ok(stream)
}

/// Reads authd's first line on `stream`.
fn answer(reader: &mut BufReader<UnixStream>) -> Result<Answer, Fail> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|err| fail(format!("authd did not answer: {}", util::strip_os_error(&err))))?;
    Ok(Answer::decode(&line))
}

/// Asks authd `request` and reads its answer.
fn ask(request: &Request) -> Result<Answer, Fail> {
    answer(&mut BufReader::new(send(request, false)?))
}

/// A line typed at the terminal after `prompt`, shown as typed; none if
/// the input ended or a signal stopped it.
fn line(prompt_text: &str) -> Result<Option<String>, Fail> {
    match prompt::ask(prompt_text, true) {
        Ok(prompt::Answer::Line(line)) => {
            Ok(Some(String::from_utf8_lossy(&line).trim().to_string()))
        }
        Ok(prompt::Answer::Stopped) => Ok(None),
        Err(err) => Err(util::failed(&"the terminal", err)),
    }
}

/// Makes the owner's account at first boot; its name and password, to
/// sign in with, or none if the input ended.
fn make_owner() -> Result<Option<(String, Vec<u8>)>, Fail> {
    println!(
        "Welcome to Dukovo. This computer has no account yet: make yours, the owner's,\n\
         which administers it. Nothing but a name and a password is needed, and the\n\
         password may be empty.\n"
    );
    loop {
        let Some(name) = line("Name: ")? else { return Ok(None) };
        if name.is_empty() {
            continue;
        }
        if let Err(why) = accounts::check_name(&name) {
            println!("{why}\n");
            continue;
        }
        let Some(full_name) = line("Full name (may be empty): ")? else { return Ok(None) };
        if let Err(why) = accounts::check_full_name(&full_name) {
            println!("{why}\n");
            continue;
        }
        let password = match Input::Terminal.new_password(&name) {
            Ok(password) => password,
            Err(why) => {
                println!("{}\n", why.message);
                continue;
            }
        };
        let request = Request::Owner { name: name.clone(), full_name, password: password.clone() };
        match ask(&request)? {
            Answer::Done(text) => {
                println!("{}\n", capitalized(&text));
                return Ok(Some((name, password)));
            }
            Answer::Refused(why) => {
                let mut password = password;
                wipe(&mut password);
                println!("{why}\n");
                // Someone made an account on another console meanwhile.
                if needs_owner()? {
                    continue;
                }
                return Ok(None);
            }
        }
    }
}

/// `text` with a capital first letter.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// Whether authd says no account can be signed in to yet.
fn needs_owner() -> Result<bool, Fail> {
    match ask(&Request::NeedsOwner)? {
        Answer::Done(text) => Ok(text == "yes"),
        Answer::Refused(why) => Err(fail(why)),
    }
}

/// Has authd start `name`'s session on this terminal, if `password` is
/// theirs, and waits for it to end. Whether it started; why not if not.
fn sign_in(name: String, password: Vec<u8>) -> Result<Result<(), String>, Fail> {
    let term = std::env::var("TERM").unwrap_or_else(|_| "dumb".into());
    let stream = send(&Request::Session { name, password, term }, true)?;
    let mut reader = BufReader::new(stream);
    match answer(&mut reader)? {
        Answer::Done(_) => {}
        Answer::Refused(why) => return Ok(Err(why)),
    }
    // The terminal is the session's now: login keeps none of it open, so
    // that it hangs up once the session's programs are done with it.
    let null = c"/devices/null";
    // SAFETY: opening /devices/null and putting it in place of the
    // terminal on 0 to 2 touches only our descriptors.
    unsafe {
        let fd = libc::open(null.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if fd >= 0 {
            for target in 0..3 {
                libc::dup2(fd, target);
            }
            libc::close(fd);
        }
    }
    // Then it waits for authd's word that the session ended, or for authd
    // to go, which ends it as far as this console is concerned.
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    Ok(Ok(()))
}

/// The administrators' names, as the account files list them, each
/// disabled one marked so: resetting its password enables it too (ADR
/// 0220).
fn administrators() -> Vec<String> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let users = accounts::users(&read(accounts::PASSWD));
    let groups = accounts::groups(&read(accounts::GROUP));
    let passwords = accounts::passwords(&read(accounts::PASSWORDS));
    let disabled = |name: &str| passwords.entries().any(|p| p.name == name && p.is_disabled());
    users
        .entries()
        .filter(|u| u.is_person() && accounts::is_admin(u, &groups))
        .map(|u| if disabled(&u.name) { format!("{} (disabled)", u.name) } else { u.name.clone() })
        .collect()
}

/// The startup-trouble menu (ADR 0253): whether to go on to the sign-in;
/// not if the input ended.
fn trouble() -> Result<bool, Fail> {
    loop {
        println!(
            "Startup trouble: this computer was started from its boot menu's\n\
             startup-trouble entry.\n\n  \
             1  Reset an administrator's password\n  \
             2  Sign in\n  \
             3  Restart\n  \
             4  Power off\n"
        );
        let Some(choice) = line("Choose 1 to 4: ")? else { return Ok(false) };
        println!();
        match choice.as_str() {
            "1" => reset()?,
            "2" | "" => return Ok(true),
            "3" => power("reboot"),
            "4" => power("poweroff"),
            _ => println!("Type 1, 2, 3 or 4.\n"),
        }
    }
}

/// Resets an administrator's password, as authd allows only while the
/// computer runs for startup trouble.
fn reset() -> Result<(), Fail> {
    let admins = administrators();
    if admins.is_empty() {
        println!("No administrator's account yet: signing in makes the owner's.\n");
        return Ok(());
    }
    println!("Administrators: {}", admins.join(", "));
    let Some(name) = line("Whose password? ")? else { return Ok(()) };
    if name.is_empty() {
        println!();
        return Ok(());
    }
    let password = match Input::Terminal.new_password(&name) {
        Ok(password) => password,
        Err(why) => {
            println!("{}\n", capitalized(&why.message));
            return Ok(());
        }
    };
    match ask(&Request::Reset { name: name.clone(), password })? {
        Answer::Done(_) => println!(
            "The password of {name} is reset. Every administrator is told at their\n\
             next sign-in, and the audit log keeps it.\n"
        ),
        Answer::Refused(why) => println!("{}.\n", capitalized(&why)),
    }
    Ok(())
}

/// Restarts or powers off the computer with `command`, reboot(8) or
/// poweroff(8), which ask Spark; the menu comes back only if it refused.
fn power(command: &str) {
    let path = format!("/system/commands/{command}");
    match std::process::Command::new(&path).status() {
        // Spark ends the sign-in with the rest.
        Ok(status) if status.success() => loop {
            std::thread::sleep(Duration::from_secs(60));
        },
        Ok(_) => println!(),
        Err(err) => println!("{path}: {}\n", util::strip_os_error(&err)),
    }
}

/// The sign-in at a console: name and password until a session starts
/// and ends.
fn console() -> Result<i32, Fail> {
    if util::own_user() != 0 {
        return Err(fail("only root signs people in: consoled(8) runs login".into()));
    }
    if util::kernel_flag(authd::STARTUP_TROUBLE) && !trouble()? {
        return Ok(1);
    }
    loop {
        if needs_owner()? {
            match make_owner()? {
                Some((name, password)) => match sign_in(name, password)? {
                    Ok(()) => return Ok(0),
                    Err(why) => println!("{why}\n"),
                },
                None => return Ok(1),
            }
            continue;
        }
        let Some(name) = line("Name: ")? else { return Ok(1) };
        if name.is_empty() {
            continue;
        }
        let Ok(password) = Input::Terminal.password("Password: ") else { return Ok(1) };
        match sign_in(name, password)? {
            Ok(()) => return Ok(0),
            Err(why) => println!("{}.\n", capitalized(&why)),
        }
    }
}

/// The first program of a session, as its user: the terminal becomes the
/// session's, then the shell runs as a login shell in the home.
fn session() -> Result<i32, Fail> {
    // SAFETY: setsid and the terminal request change only this process's
    // session; authd made it in its own process group, so setsid works.
    unsafe {
        if libc::setsid() < 0 {
            return Err(util::failed(&"setsid", io::Error::last_os_error()));
        }
        if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
            return Err(util::failed(&"the terminal", io::Error::last_os_error()));
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    if std::env::set_current_dir(&home).is_err() {
        println!("No home {home}: starting in /.");
        let _ = std::env::set_current_dir("/");
        // SAFETY: one thread; nothing else reads the environment now.
        unsafe { std::env::set_var("HOME", "/") };
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty());
    let shell = shell.unwrap_or_else(|| accounts::SHELL.into());
    // A login shell's name starts with a dash (sh(1)).
    let name = format!("-{}", shell.rsplit('/').next().unwrap_or("sh"));
    let path = CString::new(shell.clone()).map_err(|_| fail("a shell path with a NUL".into()))?;
    let arg = CString::new(name).map_err(|_| fail("a shell path with a NUL".into()))?;
    let argv = [arg.as_ptr(), std::ptr::null()];
    // SAFETY: execv takes NUL-terminated strings and a null-ended list,
    // which live until it returns, if it does.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    Err(util::failed(&shell, io::Error::last_os_error()))
}

/// login(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut in_session = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(name) if name == "session" => in_session = true,
            other => return Err(args.unknown(&other)),
        }
    }
    let _ = io::stdout().flush();
    if in_session { session() } else { console() }
}
