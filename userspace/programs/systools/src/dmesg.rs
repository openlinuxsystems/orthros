// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! dmesg(1): the kernel log, read from `/dev/kmsg` a record at a time
//! (radix-kernel-log(2)), as util-linux's dmesg prints it: each record's
//! time since boot and text, filtered by level, followed (`-w`), or
//! cleared (`-C`). Every record has facility 0, the kernel's.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use radix_abi::file;
use radix_abi::kernel_log::CLEAR;

use crate::args::{Arg, Args};
use crate::proc;
use crate::sys::{self, SYS};
use crate::util::{self, Fail, fail, usage};

/// Where the log is read.
const KMSG: &str = "/dev/kmsg";

/// syslog(3)'s levels, by number.
const LEVELS: [&str; 8] = ["emerg", "alert", "crit", "err", "warn", "notice", "info", "debug"];

/// A record of the log.
struct Line {
    level: u8,
    usec: u64,
    text: String,
}

/// Parses `/dev/kmsg`'s `PRIORITY,SEQUENCE,MICROSECONDS,-;TEXT`.
fn parse(record: &[u8]) -> Option<Line> {
    let text = String::from_utf8_lossy(record);
    let (head, body) = text.split_once(';')?;
    let mut fields = head.split(',');
    let priority: u32 = fields.next()?.parse().ok()?;
    let _seq: u64 = fields.next()?.parse().ok()?;
    let usec: u64 = fields.next()?.parse().ok()?;
    Some(Line { level: (priority & 7) as u8, usec, text: body.trim_end_matches('\n').to_string() })
}

/// A level by name or number.
fn level(name: &str) -> Option<u8> {
    if let Ok(n) = name.parse::<u8>() {
        return (n < 8).then_some(n);
    }
    match name {
        "warning" => Some(4),
        "error" => Some(3),
        "emergency" => Some(0),
        "critical" => Some(2),
        name => LEVELS.iter().position(|l| *l == name).map(|n| n as u8),
    }
}

/// Levels from a list (`err,warn`, `err+` for err and above it, `+err`
/// for err and below).
fn level_list(text: &str) -> Result<Vec<u8>, Fail> {
    let mut wanted = Vec::new();
    for name in util::list(text) {
        let (name, up, down) = match (name.strip_suffix('+'), name.strip_prefix('+')) {
            (Some(n), _) => (n, true, false),
            (_, Some(n)) => (n, false, true),
            _ => (name, false, false),
        };
        let n = level(name).ok_or_else(|| usage(format!("unknown level '{name}'")))?;
        if up {
            wanted.extend(0..=n);
        } else if down {
            wanted.extend(n..8);
        } else {
            wanted.push(n);
        }
    }
    Ok(wanted)
}

/// How the time of a record is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Time {
    Boot,
    None,
    Human,
    Delta,
}

/// dmesg(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let mut clear = false;
    let mut read_clear = false;
    let mut follow = false;
    let mut follow_new = false;
    let mut levels: Option<Vec<u8>> = None;
    let mut time = Time::Boot;
    let mut raw = false;
    let mut decode = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('C') => clear = true,
            Arg::Long(l) if l == "clear" => clear = true,
            Arg::Short('c') => read_clear = true,
            Arg::Long(l) if l == "read-clear" => read_clear = true,
            Arg::Short('w') => follow = true,
            Arg::Long(l) if l == "follow" => follow = true,
            Arg::Short('W') => follow_new = true,
            Arg::Long(l) if l == "follow-new" => follow_new = true,
            Arg::Short('l') => levels = Some(level_list(&args.text()?)?),
            Arg::Long(l) if l == "level" => levels = Some(level_list(&args.text()?)?),
            Arg::Short('t') => time = Time::None,
            Arg::Long(l) if l == "notime" => time = Time::None,
            Arg::Short('T') => time = Time::Human,
            Arg::Long(l) if l == "ctime" => time = Time::Human,
            Arg::Short('d') => time = Time::Delta,
            Arg::Long(l) if l == "show-delta" => time = Time::Delta,
            Arg::Short('r') => raw = true,
            Arg::Long(l) if l == "raw" => raw = true,
            Arg::Short('x') => decode = true,
            Arg::Long(l) if l == "decode" => decode = true,
            Arg::Short('k' | 'H' | 'L' | 'P') => {}
            Arg::Long(l) if l == "kernel" || l == "color" || l == "nopager" || l == "human" => {
                let _ = args.optional_value();
            }
            // Userspace's records: there are none of its own.
            Arg::Short('u') => return Ok(0),
            Arg::Short('n' | 'D' | 'E') => {
                return Err(fail("the console's log level cannot be set on Radix".into()));
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if clear {
        clear_log()?;
        return Ok(0);
    }
    let mut kmsg = OpenOptions::new()
        .read(true)
        .custom_flags(if follow || follow_new { 0 } else { libc::O_NONBLOCK })
        .open(KMSG)
        .map_err(|e| util::failed(&KMSG, e))?;
    if follow_new {
        // SAFETY: lseek on our own descriptor.
        unsafe { libc::lseek(kmsg.as_raw_fd(), 0, libc::SEEK_END) };
    }
    let boot_s = proc::stat().map(|s| s.boot_time_ns / 1_000_000_000).unwrap_or(0) as i64;
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut buffer = vec![0u8; 4096];
    let mut last_usec = None;
    loop {
        let n = match kmsg.read(&mut buffer) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            // A record was lost to newer ones: read on.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
            Err(e) => return Err(util::failed(&KMSG, e)),
        };
        if n == 0 {
            break;
        }
        let Some(line) = parse(&buffer[..n]) else { continue };
        if levels.as_ref().is_some_and(|l| !l.contains(&line.level)) {
            continue;
        }
        let mut text = String::new();
        if raw {
            text.push_str(&format!("<{}>", line.level));
        } else if decode {
            text.push_str(&format!("{:<6}:{:<6}: ", "kern", LEVELS[line.level as usize]));
        }
        match time {
            Time::None => {}
            Time::Boot => text.push_str(&format!(
                "[{:5}.{:06}] ",
                line.usec / 1_000_000,
                line.usec % 1_000_000
            )),
            Time::Human => {
                let at = boot_s + (line.usec / 1_000_000) as i64;
                text.push_str(&format!("[{}] ", util::local_time(at, "%a %b %e %H:%M:%S %Y")));
            }
            Time::Delta => {
                let delta = last_usec.map_or(0, |last| line.usec.saturating_sub(last));
                text.push_str(&format!(
                    "[{:5}.{:06} <{:5}.{:06}>] ",
                    line.usec / 1_000_000,
                    line.usec % 1_000_000,
                    delta / 1_000_000,
                    delta % 1_000_000
                ));
            }
        }
        last_usec = Some(line.usec);
        text.push_str(&line.text);
        writeln!(out, "{text}")?;
        if follow || follow_new {
            out.flush()?;
        }
    }
    out.flush()?;
    if read_clear {
        drop(kmsg);
        clear_log()?;
    }
    Ok(0)
}

/// Clears the log (`dmesg -C`): later reads start after what it holds.
fn clear_log() -> Result<(), Fail> {
    let kmsg: File =
        OpenOptions::new().write(true).open(KMSG).map_err(|e| util::failed(&KMSG, e))?;
    file::control(&SYS, sys::slot_of(kmsg.as_raw_fd()), CLEAR, 0, 0, 0).map_err(|s| {
        fail(format!("clear kernel buffer failed: {}", util::strip_os_error(&sys::io_error(s))))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_levels() {
        let line = parse(b"6,12,1500000,-;spark: started\n").unwrap();
        assert_eq!(line.level, 6);
        assert_eq!(line.usec, 1_500_000);
        assert_eq!(line.text, "spark: started");
        assert!(parse(b"garbage").is_none());
        assert_eq!(level("err"), Some(3));
        assert_eq!(level("warning"), Some(4));
        assert_eq!(level("7"), Some(7));
        assert_eq!(level("loud"), None);
    }
}
