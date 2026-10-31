// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What authd(8) is asked, and what it answers.
//!
//! A program connects to [`SOCKET`](crate::SOCKET), writes one request
//! and shuts its connection for writing; authd answers with one line and
//! closes it. A request is its verb and its fields, each ended by a NUL,
//! since no name or password a terminal sends holds one:
//!
//! ```text
//! check\0alice\0secret\0
//! passwd\0alice\0old secret\0new secret\0
//! add\0bob\0Bob Smith\0admin\0his secret\0
//! needs-owner\0
//! owner\0chase\0Chase\0her secret\0
//! session\0alice\0secret\0linux\0
//! elevate\0command\0none\0\0linux\0/users/alice\0LANG=C.UTF-8\0--\0dmesg\0-w\0
//! unlock\0password\0secret\0
//! forget\0
//! ```
//!
//! An answer is `ok`, maybe with words after it, or `no` and why, then a
//! newline. authd tells who asks by the user its connection gives
//! (SO_PEERCRED), never by what the request says.
//!
//! A `session` request comes with a terminal, sent with its first bytes
//! (SCM_RIGHTS), and its answer is `ok` and the number of the session's
//! first process; the connection then stays open until that process
//! ends, when authd writes [`ended`]'s line and closes it (ADR 0250).
//!
//! An `elevate` request (ADR 0219) comes with the three descriptors the
//! command is to have as its 0, 1 and 2, and a pipe's read end, sent
//! together with its first bytes. Its answer is `ok password` when a
//! password is needed and none came, or `ok` and the command's process,
//! after which the connection works as a session's: authd writes
//! [`stopped`]'s line when the command stops and [`ended`]'s when it
//! ends, and reads [`signal`]'s and [`CONTINUE`] lines from the pipe.

use crate::wipe;

/// The longest request authd reads, in bytes.
pub const REQUEST_MAX: usize = 4096;

/// A request. Its `Debug` leaves out the passwords.
#[derive(PartialEq, Eq)]
pub enum Request {
    /// Whether `password` is user `name`'s.
    Check {
        /// The user.
        name: String,
        /// The password to check.
        password: Vec<u8>,
    },
    /// Change user `name`'s password from `current` to `new`. Root, or
    /// from 13b.6 an administrator inside elevate, gives no current one.
    Passwd {
        /// The user.
        name: String,
        /// Its password now; empty from root.
        current: Vec<u8>,
        /// Its new password.
        new: Vec<u8>,
    },
    /// Make the account `name`, with its group and home: root's to ask.
    Add {
        /// Its name.
        name: String,
        /// Its full name, which may be empty.
        full_name: String,
        /// Whether it is an administrator's.
        admin: bool,
        /// Its password, which may be empty.
        password: Vec<u8>,
    },
    /// Whether the machine needs its owner's account made: `ok yes` when
    /// no account can be signed in to, `ok no` otherwise. Anyone may ask.
    NeedsOwner,
    /// Make the owner's account, an administrator's, as sign-in does at
    /// first boot: only while no account can be signed in to, and
    /// root's to ask.
    Owner {
        /// Its name.
        name: String,
        /// Its full name, which may be empty.
        full_name: String,
        /// Its password, which may be empty.
        password: Vec<u8>,
    },
    /// Sign `name` in on the terminal sent with the request: start their
    /// shell there, as them, if `password` is theirs. Root's to ask, as
    /// sign-in programs run as root.
    Session {
        /// The user.
        name: String,
        /// The password to check.
        password: Vec<u8>,
        /// The terminal's type, for `TERM`.
        term: String,
    },
    /// Run a command as an administrator, with the admin set (ADR 0219):
    /// an administrator's to ask, and after their password unless their
    /// terminal pane typed it in the last 15 minutes.
    Elevate {
        /// What runs: a command, or root's shell.
        mode: Mode,
        /// The asker's password, if it typed one.
        password: Option<Vec<u8>>,
        /// The terminal's type, for `TERM`.
        term: String,
        /// The directory the command starts in.
        directory: Vec<u8>,
        /// The asker's language settings, `LANG`, `LANGUAGE` and `LC_*`,
        /// as `NAME=value`.
        language: Vec<String>,
        /// The command and its arguments: for a shell, what it runs with
        /// `-c`, if anything.
        command: Vec<Vec<u8>>,
    },
    /// Unlock elevate in the asker's terminal pane for 15 minutes, or for
    /// 15 more, as `sudo -v` does: after the password, unless it is
    /// unlocked already.
    Unlock {
        /// The asker's password, if it typed one.
        password: Option<Vec<u8>>,
    },
    /// Lock elevate in the asker's terminal pane again, as `sudo -k`
    /// does. Anyone may ask.
    Forget,
}

/// What an [`Request::Elevate`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The command, found in the system's own commands.
    Command,
    /// Root's shell, in the directory asked for (`sudo -s`).
    Shell,
    /// Root's shell as a login shell, in root's home (`sudo -i`).
    Login,
}

impl Mode {
    fn word(self) -> &'static [u8] {
        match self {
            Mode::Command => b"command",
            Mode::Shell => b"shell",
            Mode::Login => b"login",
        }
    }

    fn of(word: &[u8]) -> Option<Mode> {
        match word {
            b"command" => Some(Mode::Command),
            b"shell" => Some(Mode::Shell),
            b"login" => Some(Mode::Login),
            _ => None,
        }
    }
}

/// What `ok` says to an `elevate` or `unlock` request that needs a
/// password and came without one.
pub const NEEDS_PASSWORD: &str = "password";

/// Why a wrong password, or a name there is not, is refused: the same
/// words for both.
pub const WRONG_PASSWORD: &str = "the name or the password is wrong";

/// The fields of an optional password: whether there is one, then it.
fn password_fields(password: &Option<Vec<u8>>) -> [&[u8]; 2] {
    match password {
        Some(password) => [b"password", password],
        None => [b"none", b""],
    }
}

/// The optional password in `kind` and `password`, as
/// [`password_fields`] made them.
fn password_of(kind: &[u8], password: &[u8]) -> Option<Option<Vec<u8>>> {
    match kind {
        b"password" => Some(Some(password.to_vec())),
        b"none" if password.is_empty() => Some(None),
        _ => None,
    }
}

impl Request {
    /// What is sent.
    pub fn encode(&self) -> Vec<u8> {
        let fields: Vec<&[u8]> = match self {
            Request::Check { name, password } => vec![b"check", name.as_bytes(), password],
            Request::Passwd { name, current, new } => {
                vec![b"passwd", name.as_bytes(), current, new]
            }
            Request::Add { name, full_name, admin, password } => vec![
                b"add",
                name.as_bytes(),
                full_name.as_bytes(),
                if *admin { b"admin" } else { b"user" },
                password,
            ],
            Request::NeedsOwner => vec![b"needs-owner"],
            Request::Owner { name, full_name, password } => {
                vec![b"owner", name.as_bytes(), full_name.as_bytes(), password]
            }
            Request::Session { name, password, term } => {
                vec![b"session", name.as_bytes(), password, term.as_bytes()]
            }
            Request::Elevate { mode, password, term, directory, language, command } => {
                let mut fields: Vec<&[u8]> = vec![b"elevate", mode.word()];
                fields.extend(password_fields(password));
                fields.extend([term.as_bytes(), directory.as_slice()]);
                fields.extend(language.iter().map(String::as_bytes));
                fields.push(b"--");
                fields.extend(command.iter().map(Vec::as_slice));
                fields
            }
            Request::Unlock { password } => {
                let [kind, password] = password_fields(password);
                vec![b"unlock", kind, password]
            }
            Request::Forget => vec![b"forget"],
        };
        let mut bytes = Vec::new();
        for field in fields {
            bytes.extend_from_slice(field);
            bytes.push(0);
        }
        bytes
    }

    /// The request in `bytes`, all of what came.
    pub fn decode(bytes: &[u8]) -> Result<Request, String> {
        let malformed = || "a request authd does not know".to_string();
        if bytes.len() > REQUEST_MAX {
            return Err("a request longer than authd reads".into());
        }
        let body = bytes.strip_suffix(&[0]).ok_or_else(malformed)?;
        let fields: Vec<&[u8]> = body.split(|&b| b == 0).collect();
        let text = |field: &[u8]| String::from_utf8(field.to_vec()).map_err(|_| malformed());
        match fields[..] {
            [b"check", name, password] => {
                Ok(Request::Check { name: text(name)?, password: password.to_vec() })
            }
            [b"passwd", name, current, new] => Ok(Request::Passwd {
                name: text(name)?,
                current: current.to_vec(),
                new: new.to_vec(),
            }),
            [b"add", name, full_name, kind @ (b"admin" | b"user"), password] => Ok(Request::Add {
                name: text(name)?,
                full_name: text(full_name)?,
                admin: kind == b"admin",
                password: password.to_vec(),
            }),
            [b"needs-owner"] => Ok(Request::NeedsOwner),
            [b"owner", name, full_name, password] => Ok(Request::Owner {
                name: text(name)?,
                full_name: text(full_name)?,
                password: password.to_vec(),
            }),
            [b"session", name, password, term] => Ok(Request::Session {
                name: text(name)?,
                password: password.to_vec(),
                term: text(term)?,
            }),
            [b"elevate", mode, kind, password, term, directory, ref rest @ ..] => {
                let mode = Mode::of(mode).ok_or_else(malformed)?;
                let password = password_of(kind, password).ok_or_else(malformed)?;
                // The language settings, up to the first `--`; the command
                // after it.
                let split = rest.iter().position(|f| *f == b"--").ok_or_else(malformed)?;
                let language = rest[..split]
                    .iter()
                    .map(|f| text(f).ok().filter(|t| t.contains('=')).ok_or_else(malformed))
                    .collect::<Result<Vec<_>, _>>()?;
                let command: Vec<Vec<u8>> = rest[split + 1..].iter().map(|f| f.to_vec()).collect();
                if mode == Mode::Command && command.is_empty() {
                    return Err(malformed());
                }
                Ok(Request::Elevate {
                    mode,
                    password,
                    term: text(term)?,
                    directory: directory.to_vec(),
                    language,
                    command,
                })
            }
            [b"unlock", kind, password] => {
                Ok(Request::Unlock { password: password_of(kind, password).ok_or_else(malformed)? })
            }
            [b"forget"] => Ok(Request::Forget),
            _ => Err(malformed()),
        }
    }

    /// Overwrites the passwords it holds.
    pub fn wipe(&mut self) {
        match self {
            Request::Check { password, .. }
            | Request::Add { password, .. }
            | Request::Owner { password, .. }
            | Request::Session { password, .. } => wipe(password),
            Request::Passwd { current, new, .. } => {
                wipe(current);
                wipe(new);
            }
            Request::Elevate { password, .. } | Request::Unlock { password } => {
                if let Some(password) = password {
                    wipe(password);
                }
            }
            Request::NeedsOwner | Request::Forget => {}
        }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Check { name, .. } => write!(f, "Check {{ name: {name:?} }}"),
            Request::Passwd { name, .. } => write!(f, "Passwd {{ name: {name:?} }}"),
            Request::Add { name, full_name, admin, .. } => {
                write!(f, "Add {{ name: {name:?}, full_name: {full_name:?}, admin: {admin} }}")
            }
            Request::NeedsOwner => write!(f, "NeedsOwner"),
            Request::Owner { name, full_name, .. } => {
                write!(f, "Owner {{ name: {name:?}, full_name: {full_name:?} }}")
            }
            Request::Session { name, term, .. } => {
                write!(f, "Session {{ name: {name:?}, term: {term:?} }}")
            }
            Request::Elevate { mode, password, term, command, .. } => {
                let typed = password.is_some();
                let command: Vec<_> = command.iter().map(|w| String::from_utf8_lossy(w)).collect();
                write!(
                    f,
                    "Elevate {{ mode: {mode:?}, password: {typed}, term: {term:?}, command: \
                     {command:?} }}"
                )
            }
            Request::Unlock { password } => {
                write!(f, "Unlock {{ password: {} }}", password.is_some())
            }
            Request::Forget => write!(f, "Forget"),
        }
    }
}

/// authd's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Done, with what there is to say.
    Done(String),
    /// Not done, and why, in words for the person who asked.
    Refused(String),
}

impl Answer {
    /// Its line.
    pub fn encode(&self) -> String {
        let one_line = |text: &str| text.replace(['\n', '\r'], " ");
        match self {
            Answer::Done(text) if text.is_empty() => "ok\n".into(),
            Answer::Done(text) => format!("ok {}\n", one_line(text)),
            Answer::Refused(why) => format!("no {}\n", one_line(why)),
        }
    }

    /// The answer in `line`; one authd would not give is a refusal that
    /// says so.
    pub fn decode(line: &str) -> Answer {
        let line = line.trim_end_matches('\n');
        match line.split_once(' ').unwrap_or((line, "")) {
            ("ok", text) => Answer::Done(text.into()),
            ("no", why) if !why.is_empty() => Answer::Refused(why.into()),
            _ => Answer::Refused(format!("authd answered `{line}`")),
        }
    }
}

/// The line authd writes on a `session` connection when the session's
/// first process ends with wait status `status`.
pub fn ended(status: i32) -> String {
    format!("ended {status}\n")
}

/// The wait status in a line [`ended`] made.
pub fn ended_status(line: &str) -> Option<i32> {
    line.trim_end_matches('\n').strip_prefix("ended ")?.parse().ok()
}

/// The line authd writes on an `elevate` connection when the command
/// stops with signal `signal`, as Ctrl+Z stops it.
pub fn stopped(signal: i32) -> String {
    format!("stopped {signal}\n")
}

/// The signal in a line [`stopped`] made.
pub fn stopped_signal(line: &str) -> Option<i32> {
    line.trim_end_matches('\n').strip_prefix("stopped ")?.parse().ok()
}

/// The line elevate writes to its pipe to authd to have it send the
/// command `signal`, as a terminal would.
pub fn signal(signal: i32) -> String {
    format!("signal {signal}\n")
}

/// The line elevate writes to its pipe to authd to have the stopped
/// command go on, as `fg` has elevate do.
pub const CONTINUE: &str = "continue\n";

/// What a line elevate wrote to its pipe asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relay {
    /// Send the command this signal.
    Signal(i32),
    /// Have the stopped command go on.
    Continue,
}

impl Relay {
    /// The line `line`, without its newline, asks for, if it is one of
    /// [`signal`]'s or [`CONTINUE`].
    pub fn decode(line: &str) -> Option<Relay> {
        if line == CONTINUE.trim_end() {
            return Some(Relay::Continue);
        }
        let number: i32 = line.strip_prefix("signal ")?.parse().ok()?;
        (1..=64).contains(&number).then_some(Relay::Signal(number))
    }
}
