// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `service check`, `service new` and `service edit` (service(8)): the
//! service files Spark reads (service.toml(5), ADR 0216), checked with the
//! same reader Spark uses.
//!
//! `service edit` works as visudo(8) does: it copies the file, opens the
//! copy in the editor, checks it, and puts it in place only if it reads;
//! otherwise it shows what is wrong and offers to edit it again.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use manifest::service;

use crate::util::{self, Fail, fail, usage};

/// Where Spark reads the service files.
pub const DIR: &str = "/system/settings/spark/services";

/// The editor `service edit` runs when neither VISUAL nor EDITOR names
/// one.
const EDITOR: &str = "nano";

/// The file `what` names: a path if it holds a `/` or ends in `.toml`,
/// else a service's name, whose file is in [`DIR`].
fn path_of(what: &str) -> Result<PathBuf, Fail> {
    if what.contains('/') || Path::new(what).extension().is_some_and(|e| e == "toml") {
        return Ok(PathBuf::from(what));
    }
    service::check_name(what).map_err(fail)?;
    Ok(Path::new(DIR).join(format!("{what}.toml")))
}

/// What is wrong with the service file at `path`, or None if it reads.
/// Its name must be a service's name too.
fn check_file(path: &Path) -> Result<Option<String>, Fail> {
    let text = fs::read(path).map_err(|e| util::failed(&path.display(), e))?;
    let Ok(text) = String::from_utf8(text) else {
        return Ok(Some("it is not UTF-8 text".into()));
    };
    let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    if let Err(err) = service::check_name(name) {
        return Ok(Some(format!("its name: {err}")));
    }
    Ok(service::from_toml(&text).err().map(|err| err.to_string()))
}

/// `service check [NAME | FILE]...`: checks each file, or every one in
/// [`DIR`]; fails if one does not read.
pub fn check(operands: &[String]) -> Result<i32, Fail> {
    let mut paths = Vec::new();
    if operands.is_empty() {
        let entries = fs::read_dir(DIR).map_err(|e| util::failed(&DIR, e))?;
        let mut names: Vec<String> =
            entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for name in names {
            let extension = Path::new(&name).extension().and_then(|e| e.to_str());
            if extension == Some("toml") {
                paths.push(Path::new(DIR).join(name));
            } else if extension == Some("service") {
                println!(
                    "{DIR}/{name}: a file of before Phase 13; Spark converts it to TOML when it \
                     next starts"
                );
            }
        }
    }
    for operand in operands {
        paths.push(path_of(operand)?);
    }
    let mut bad = 0;
    for path in &paths {
        match check_file(path) {
            Ok(None) => println!("{}: ok", path.display()),
            Ok(Some(why)) => {
                eprintln!("{}: {why}", path.display());
                bad += 1;
            }
            Err(failed) => {
                eprintln!("service: {}", failed.message);
                bad += 1;
            }
        }
    }
    Ok(i32::from(bad > 0))
}

/// `service new NAME`: writes the commented template for NAME, which is
/// not started at boot until its file says so.
pub fn new(operands: &[String]) -> Result<i32, Fail> {
    let [name] = operands else {
        return Err(usage("Usage: service new NAME".into()));
    };
    let path = path_of(name)?;
    if path.parent() != Some(Path::new(DIR)) {
        return Err(usage(format!("service new takes a service's name, not {name}")));
    }
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path).map_err(
            |e| match e.kind() {
                io::ErrorKind::AlreadyExists => fail(format!(
                    "{} is there already; edit it with `service edit {name}`",
                    path.display()
                )),
                _ => util::failed(&path.display(), e),
            },
        )?;
    file.write_all(service::template(name).as_bytes())
        .map_err(|e| util::failed(&path.display(), e))?;
    println!(
        "wrote {}: edit it with `service edit {name}`, then start it with `service {name} start`",
        path.display()
    );
    Ok(0)
}

/// `service edit NAME`: edits NAME's file through a copy, put in place
/// only once it reads.
pub fn edit(operands: &[String]) -> Result<i32, Fail> {
    let [name] = operands else {
        return Err(usage("Usage: service edit NAME".into()));
    };
    let path = path_of(name)?;
    let original = fs::read(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => fail(format!(
            "{} is not there; write a new one with `service new {name}`",
            path.display()
        )),
        _ => util::failed(&path.display(), e),
    })?;
    // The copy is beside the file, so that putting it in place is a
    // rename; that it exists already means another edit is going on.
    let dir = path.parent().unwrap_or(Path::new("."));
    let file_name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let copy = dir.join(format!(".{file_name}.edit"));
    let mode = fs::metadata(&path).map_or(0o644, |m| m.permissions().mode() & 0o7777);
    let mut out = OpenOptions::new().write(true).create_new(true).mode(mode).open(&copy).map_err(
        |e| match e.kind() {
            io::ErrorKind::AlreadyExists => fail(format!(
                "{} is being edited already: {} is its copy",
                path.display(),
                copy.display()
            )),
            _ => util::failed(&copy.display(), e),
        },
    )?;
    let written = out.write_all(&original);
    drop(out);
    let result = written
        .map_err(|e| util::failed(&copy.display(), e))
        .and_then(|()| edit_copy(&path, &copy, &original, name));
    let _ = fs::remove_file(&copy);
    result
}

/// Runs the editor on `copy` until it reads or the user gives up, then
/// puts it in place of `path`.
fn edit_copy(path: &Path, copy: &Path, original: &[u8], name: &str) -> Result<i32, Fail> {
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
        .unwrap_or_else(|| EDITOR.into());
    let stdin = io::stdin();
    loop {
        // Through the shell, so that EDITOR may hold arguments, as
        // `vi -n`; the file is $1, never part of the command.
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(copy)
            .status()
            .map_err(|e| util::failed(&editor, e))?;
        if !status.success() {
            return Err(fail(format!("{editor} failed; {} is as it was", path.display())));
        }
        let edited = fs::read(copy).map_err(|e| util::failed(&copy.display(), e))?;
        if edited == original {
            println!("{}: no change", path.display());
            return Ok(0);
        }
        let why = match String::from_utf8(edited) {
            Ok(text) => service::from_toml(&text).err().map(|err| err.to_string()),
            Err(_) => Some("it is not UTF-8 text".into()),
        };
        let Some(why) = why else {
            fs::rename(copy, path).map_err(|e| util::failed(&path.display(), e))?;
            println!(
                "{}: ok; Spark reads it when the service next starts: `service {name} restart`",
                path.display()
            );
            return Ok(0);
        };
        eprintln!("{}: {why}", path.display());
        eprint!("What now? (e)dit again, or e(x)it without saving: ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        let read = stdin.lock().read_line(&mut answer).unwrap_or(0);
        if read == 0 || !answer.trim_start().starts_with('e') {
            eprintln!("{} is as it was", path.display());
            return Ok(1);
        }
    }
}

/// `service secret set NAME SECRET`, `service secret list NAME` and
/// `service secret remove NAME SECRET` (ADR 0217): the secrets Spark
/// hands service NAME as read-only files, which no command shows back.
pub fn secret(operands: &[String]) -> Result<i32, Fail> {
    const USAGE: &str = "Usage: service secret set NAME SECRET | list NAME | remove NAME SECRET";
    let (verb, rest) = operands.split_first().ok_or_else(|| usage(USAGE.into()))?;
    let (name, secret) = match (verb.as_str(), rest) {
        ("list", [name]) => (name, None),
        ("set" | "remove", [name, secret]) => (name, Some(secret.as_str())),
        _ => return Err(usage(USAGE.into())),
    };
    service::check_name(name).map_err(fail)?;
    if let Some(secret) = secret {
        service::check_secret(secret).map_err(fail)?;
    }
    let dir = PathBuf::from(service::secrets_dir(name));
    match (verb.as_str(), secret) {
        ("set", Some(secret)) => {
            let value = read_secret(name, secret)?;
            // Only root reads them; Spark hands each service its own.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)
                .map_err(|e| util::failed(&dir.display(), e))?;
            let path = dir.join(secret);
            let new = dir.join(format!(".{secret}.new"));
            let _ = fs::remove_file(&new);
            let written = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&new)
                .and_then(|mut file| file.write_all(&value).and_then(|()| file.sync_all()))
                .and_then(|()| fs::rename(&new, &path));
            if let Err(e) = written {
                let _ = fs::remove_file(&new);
                return Err(util::failed(&path.display(), e));
            }
            println!(
                "set {name}'s secret {secret}; {name} reads it when it next starts: `service \
                 {name} restart`"
            );
        }
        ("remove", Some(secret)) => {
            let path = dir.join(secret);
            fs::remove_file(&path).map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => fail(format!("{name} has no secret {secret}")),
                _ => util::failed(&path.display(), e),
            })?;
        }
        _ => {
            let mut names: Vec<String> = match fs::read_dir(&dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.'))
                    .collect(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(util::failed(&dir.display(), e)),
            };
            names.sort();
            for secret in names {
                println!("{secret}");
            }
        }
    }
    Ok(0)
}

/// Set by SIGINT, SIGTERM or SIGHUP while a secret is typed, so that the
/// terminal's echo comes back before `service` ends.
static STOPPED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOPPED.store(true, Ordering::Relaxed);
}

/// The value of `name`'s secret `secret`, from the standard input: a line
/// typed at the terminal, which is not shown, or else everything there,
/// with one newline at its end taken off, as `echo` adds one.
fn read_secret(name: &str, secret: &str) -> Result<Vec<u8>, Fail> {
    let mut value = Vec::new();
    // SAFETY: isatty only looks at the descriptor.
    if unsafe { libc::isatty(0) } == 1 {
        eprint!("{name}'s secret {secret}: ");
        let _ = io::stderr().flush();
        // Ctrl+C stops the read, not `service`, which puts the echo back
        // first; without SA_RESTART, read(2) ends with EINTR.
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: all zeros is a valid sigaction: numbers, a signal
            // set and a function address.
            let mut action: libc::sigaction = unsafe { util::zeroed() };
            action.sa_sigaction = on_signal as *const () as usize;
            // SAFETY: the handler only stores to an atomic.
            unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
        }
        // SAFETY: all zeros is a valid termios: numbers.
        let mut saved: libc::termios = unsafe { util::zeroed() };
        // SAFETY: tcgetattr fills `saved`.
        let got = unsafe { libc::tcgetattr(0, &raw mut saved) } == 0;
        if got {
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            // SAFETY: `quiet` is a valid termios.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw const quiet) };
        }
        let read = read_line(&mut value);
        if got {
            // SAFETY: `saved` is the termios read above.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw const saved) };
        }
        eprintln!();
        if STOPPED.load(Ordering::Relaxed) {
            return Err(fail(format!("stopped: {name}'s secret {secret} is as it was")));
        }
        read.map_err(|e| util::failed(&"the terminal", e))?;
    } else {
        io::stdin()
            .lock()
            .read_to_end(&mut value)
            .map_err(|e| util::failed(&"standard input", e))?;
    }
    if value.ends_with(b"\n") {
        value.pop();
    }
    if value.is_empty() {
        return Err(fail(format!("{name}'s secret {secret} cannot be empty")));
    }
    Ok(value)
}

/// Reads the standard input a byte at a time into `line` up to its
/// newline, which it keeps, or its end; stops early when a signal came.
fn read_line(line: &mut Vec<u8>) -> io::Result<()> {
    let mut byte = 0u8;
    while !STOPPED.load(Ordering::Relaxed) {
        // SAFETY: `byte` is one writable byte.
        match unsafe { libc::read(0, (&raw mut byte).cast(), 1) } {
            0 => break,
            1 => {
                line.push(byte);
                if byte == b'\n' {
                    break;
                }
            }
            _ => match io::Error::last_os_error() {
                err if err.kind() == io::ErrorKind::Interrupted => {}
                err => return Err(err),
            },
        }
    }
    Ok(())
}
