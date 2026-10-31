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
//! ```
//!
//! An answer is `ok`, maybe with words after it, or `no` and why, then a
//! newline. authd tells who asks by the user its connection gives
//! (SO_PEERCRED), never by what the request says.

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
            _ => Err(malformed()),
        }
    }

    /// Overwrites the passwords it holds.
    pub fn wipe(&mut self) {
        match self {
            Request::Check { password, .. } | Request::Add { password, .. } => wipe(password),
            Request::Passwd { current, new, .. } => {
                wipe(current);
                wipe(new);
            }
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
