// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! logger(1): writes a line to the system's log, as util-linux's logger
//! does, for scripts.
//!
//! Each message goes to `/devices/kmsg` as one line with its `<N>` in
//! front. The kernel starts the record with the writing thread's name and
//! marks it with the process and the user it acts for, which a script
//! cannot fake (ADR 0252), so logger names its thread after the tag
//! first. logd(8) keeps the record, and its writer reads it back with
//! log(1), as root does.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};

use crate::args::{Arg, Args};
use crate::util::{self, Fail, fail, usage};

/// Where the lines go.
const KMSG: &str = "/devices/kmsg";
/// The most bytes of a thread's name, which the kernel puts in front of
/// each record: Linux's 15 (radix-process(2)).
const NAME_MAX: usize = 15;

/// syslog(3)'s facilities, by name, with util-linux's `security` for auth.
const FACILITIES: [(&str, u8); 25] = [
    ("kern", 0),
    ("user", 1),
    ("mail", 2),
    ("daemon", 3),
    ("auth", 4),
    ("security", 4),
    ("syslog", 5),
    ("lpr", 6),
    ("news", 7),
    ("uucp", 8),
    ("cron", 9),
    ("authpriv", 10),
    ("ftp", 11),
    ("local0", 16),
    ("local1", 17),
    ("local2", 18),
    ("local3", 19),
    ("local4", 20),
    ("local5", 21),
    ("local6", 22),
    ("local7", 23),
    ("res0", 12),
    ("res1", 13),
    ("res2", 14),
    ("res3", 15),
];

/// syslog(3)'s levels, by name, with the old names util-linux takes.
const LEVELS: [(&str, u8); 11] = [
    ("emerg", 0),
    ("panic", 0),
    ("alert", 1),
    ("crit", 2),
    ("err", 3),
    ("error", 3),
    ("warning", 4),
    ("warn", 4),
    ("notice", 5),
    ("info", 6),
    ("debug", 7),
];

/// A priority as `-p` takes it: `facility.level`, a level alone (of the
/// user facility), or a number. util-linux's default is `user.notice`.
fn priority(text: &str) -> Result<u8, Fail> {
    let bad = || usage(format!("unknown priority '{text}'"));
    if let Ok(n) = text.parse::<u8>() {
        return if n < 192 { Ok(n) } else { Err(bad()) };
    }
    let (facility, level) = match text.split_once('.') {
        Some((facility, level)) => (Some(facility), level),
        None => (None, text),
    };
    let facility = match facility {
        Some(name) => FACILITIES.iter().find(|f| f.0 == name).ok_or_else(bad)?.1,
        None => 1,
    };
    // The kernel's facility is the kernel's own: no program writes as it.
    if facility == 0 {
        return Err(usage("the kern facility is the kernel's own".into()));
    }
    let level = LEVELS.iter().find(|l| l.0 == level).ok_or_else(bad)?.1;
    Ok(facility * 8 + level)
}

/// The tag cut to what a thread's name holds, at a character's edge, as
/// Linux cuts a program's name.
fn thread_name(tag: &str) -> &str {
    let mut end = tag.len().min(NAME_MAX);
    while !tag.is_char_boundary(end) {
        end -= 1;
    }
    &tag[..end]
}

/// The line written for `message`: its priority in front, and a control
/// character as a blank, so that a message stays one record.
fn line(priority: u8, message: &str) -> Vec<u8> {
    let mut line = format!("<{priority}>").into_bytes();
    line.extend(
        message.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().bytes(),
    );
    line.push(b'\n');
    line
}

/// logger(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut tag: Option<String> = None;
    let mut pri = 13;
    let mut stderr = false;
    let mut file: Option<String> = None;
    let mut skip_empty = false;
    let mut no_act = false;
    let mut words: Vec<String> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('t') => tag = Some(args.text()?),
            Arg::Long(l) if l == "tag" => tag = Some(args.text()?),
            Arg::Short('p') => pri = priority(&args.text()?)?,
            Arg::Long(l) if l == "priority" => pri = priority(&args.text()?)?,
            Arg::Short('s') => stderr = true,
            Arg::Long(l) if l == "stderr" => stderr = true,
            Arg::Short('f') => file = Some(args.text()?),
            Arg::Long(l) if l == "file" => file = Some(args.text()?),
            Arg::Short('e') => skip_empty = true,
            Arg::Long(l) if l == "skip-empty" => skip_empty = true,
            Arg::Long(l) if l == "no-act" => no_act = true,
            // The kernel marks every record with its process anyway.
            Arg::Short('i') => {}
            Arg::Long(l) if l == "id" => {
                let _ = args.optional_value();
            }
            Arg::Short('n' | 'P' | 'u') => {
                let _ = args.value()?;
                return Err(remote());
            }
            Arg::Long(l) if ["server", "port", "socket"].contains(&&*l) => {
                let _ = args.value()?;
                return Err(remote());
            }
            Arg::Short('d' | 'T') => return Err(remote()),
            Arg::Long(l) if l == "udp" || l == "tcp" => return Err(remote()),
            Arg::Operand(word) => words.push(word.to_string_lossy().into_owned()),
            other => return Err(args.unknown(&other)),
        }
    }
    let tag = tag.unwrap_or_else(|| util::user_name(util::own_user()));
    let name = thread_name(&tag);
    if let Ok(name) = CString::new(name) {
        // SAFETY: names this thread with a C string of at most 15 bytes.
        unsafe { libc::pthread_setname_np(libc::pthread_self(), name.as_ptr()) };
    }
    let mut kmsg = if no_act {
        None
    } else {
        Some(OpenOptions::new().write(true).open(KMSG).map_err(|e| util::failed(&KMSG, e))?)
    };
    let mut log = |message: &str| -> Result<(), Fail> {
        if let Some(kmsg) = kmsg.as_mut() {
            kmsg.write_all(&line(pri, message)).map_err(|e| util::failed(&KMSG, e))?;
        }
        if stderr {
            eprintln!("<{pri}>{tag}: {message}");
        }
        Ok(())
    };
    if !words.is_empty() {
        log(&words.join(" "))?;
        return Ok(0);
    }
    let input: Box<dyn BufRead> = match &file {
        Some(path) => {
            Box::new(io::BufReader::new(File::open(path).map_err(|e| util::failed(path, e))?))
        }
        None => Box::new(io::stdin().lock()),
    };
    for message in input.lines() {
        let message = message.map_err(|e| fail(util::strip_os_error(&e)))?;
        if skip_empty && message.trim().is_empty() {
            continue;
        }
        log(&message)?;
    }
    Ok(0)
}

/// The refusal of a remote log: logger writes to this machine's.
fn remote() -> Fail {
    usage("only this machine's log is written to; there is no syslog server".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities() {
        assert_eq!(priority("user.notice").unwrap(), 13);
        assert_eq!(priority("err").unwrap(), 11);
        assert_eq!(priority("local3.debug").unwrap(), 159);
        assert_eq!(priority("authpriv.warn").unwrap(), 84);
        assert_eq!(priority("security.info").unwrap(), 38);
        assert_eq!(priority("14").unwrap(), 14);
        for bad in ["kern.err", "user.loud", "postal.info", "192", ""] {
            assert!(priority(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lines_and_names() {
        assert_eq!(line(13, "backup done"), b"<13>backup done\n");
        assert_eq!(line(14, "a\tb\nc"), b"<14>a b c\n");
        assert_eq!(thread_name("backup"), "backup");
        assert_eq!(thread_name("a-very-long-tag-indeed"), "a-very-long-tag");
        // 14 bytes, then a two-byte character that would not fit.
        assert_eq!(thread_name("abcdefghijklmné"), "abcdefghijklmn");
    }
}
