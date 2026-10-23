// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! renice(1), ionice(1) and taskset(1): a process's nice level, its I/O
//! class and level, and the CPUs its threads may run on, as util-linux's
//! tools set them. ionice and taskset call the kernel directly
//! (radix-process(2)'s `io_priority`, `set_affinity` and `affinity`).

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;

use radix_abi::process::{self as abi, IoClass, PriorityWhich};

use crate::args::{Arg, Args};
use crate::proc;
use crate::sys::{self, SLOT_PROCESS, SYS};
use crate::util::{self, Fail, fail, usage};

/// Replaces this program with `command`, as the tools that run one do;
/// returns only if that fails.
pub fn exec(command: &[OsString]) -> Fail {
    let Some(name) = command.first() else { return usage("no command given".into()) };
    let c: Vec<CString> = command.iter().filter_map(|a| CString::new(a.as_bytes()).ok()).collect();
    let mut argv: Vec<*const libc::c_char> = c.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: argv is a null-terminated array of C strings that outlive
    // the call.
    unsafe { libc::execvp(argv[0], argv.as_ptr()) };
    let e = io::Error::last_os_error();
    let code = if e.raw_os_error() == Some(libc::ENOENT) { 127 } else { 126 };
    util::fail_with(
        code,
        format!("failed to execute {}: {}", name.to_string_lossy(), util::strip_os_error(&e)),
    )
}

/// renice(1): `renice [-n] PRIORITY [[-p] PID...] [-g PGRP...] [-u USER...]`.
pub fn renice(args: &mut Args) -> Result<i32, Fail> {
    let items: Vec<String> =
        args.rest().into_iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut items = items.into_iter().peekable();
    let mut adjustment: Option<i32> = None;
    let mut relative = false;
    // PRIO_PROCESS and the others are unsigned in some C libraries.
    let mut which = libc::PRIO_PROCESS as i64;
    let mut code = 0;
    let mut any = false;
    while let Some(item) = items.next() {
        match item.as_str() {
            "-n" | "--priority" | "--relative" => {
                // -n sets the level, as util-linux's does, unless
                // POSIXLY_CORRECT asks for POSIX's adjustment.
                relative = item == "--relative"
                    || item == "-n" && std::env::var_os("POSIXLY_CORRECT").is_some();
                let n = items
                    .next()
                    .ok_or_else(|| usage("option requires an argument -- 'n'".into()))?;
                adjustment = Some(n.parse().map_err(|_| usage(format!("invalid priority '{n}'")))?);
            }
            "-p" | "--pid" => which = libc::PRIO_PROCESS as i64,
            "-g" | "--pgrp" => which = libc::PRIO_PGRP as i64,
            "-u" | "--user" => which = libc::PRIO_USER as i64,
            "-h" | "--help" => {
                println!("usage: renice [-n] PRIORITY [-p|-g|-u] ID...");
                return Ok(0);
            }
            _ if adjustment.is_none() => {
                adjustment =
                    Some(item.parse().map_err(|_| usage(format!("invalid priority '{item}'")))?);
            }
            _ => {
                any = true;
                let who: u32 = if which == libc::PRIO_USER as i64 {
                    util::user_id(&item).ok_or_else(|| usage(format!("unknown user {item}")))?
                } else {
                    item.parse().map_err(|_| usage(format!("bad value {item}")))?
                };
                let n = adjustment.unwrap_or(0);
                // SAFETY: errno is the calling thread's; getpriority and
                // setpriority read nothing of the program's.
                let old = unsafe {
                    *libc::__errno_location() = 0;
                    libc::getpriority(which as _, who as _)
                };
                // SAFETY: as above.
                if old == -1 && unsafe { *libc::__errno_location() } != 0 {
                    eprintln!(
                        "renice: failed to get priority for {item} ({}): {}",
                        kind(which),
                        util::strip_os_error(&io::Error::last_os_error())
                    );
                    code = 1;
                    continue;
                }
                let new = if relative { old + n } else { n }.clamp(-20, 19);
                // SAFETY: as above.
                if unsafe { libc::setpriority(which as _, who as _, new) } != 0 {
                    eprintln!(
                        "renice: failed to set priority for {item} ({}): {}",
                        kind(which),
                        util::strip_os_error(&io::Error::last_os_error())
                    );
                    code = 1;
                    continue;
                }
                println!("{item} ({}) old priority {old}, new priority {new}", kind(which));
            }
        }
    }
    if adjustment.is_none() || !any {
        return Err(usage("usage: renice [-n] PRIORITY [-p|-g|-u] ID...".into()));
    }
    Ok(code)
}

fn kind(which: i64) -> &'static str {
    if which == libc::PRIO_PGRP as i64 {
        "process group ID"
    } else if which == libc::PRIO_USER as i64 {
        "user ID"
    } else {
        "process ID"
    }
}

/// A class by name or number.
fn io_class(text: &str) -> Option<IoClass> {
    match text.to_ascii_lowercase().as_str() {
        "0" | "none" => Some(IoClass::None),
        "1" | "realtime" | "rt" => Some(IoClass::Realtime),
        "2" | "best-effort" | "be" | "best_effort" => Some(IoClass::BestEffort),
        "3" | "idle" => Some(IoClass::Idle),
        _ => None,
    }
}

/// A class's name, as ionice prints it.
fn class_name(class: IoClass) -> &'static str {
    match class {
        IoClass::None => "none",
        IoClass::Realtime => "realtime",
        IoClass::BestEffort => "best-effort",
        IoClass::Idle => "idle",
    }
}

/// ionice(1).
pub fn ionice(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut class: Option<IoClass> = None;
    let mut level: Option<u32> = None;
    let mut which = PriorityWhich::Process;
    let mut who: Vec<u64> = Vec::new();
    let mut tolerant = false;
    let mut targeted = false;
    let mut command: Vec<OsString> = Vec::new();
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('c') => {
                let text = args.text()?;
                class = Some(
                    io_class(&text)
                        .ok_or_else(|| usage(format!("unknown scheduling class: '{text}'")))?,
                );
            }
            Arg::Long(l) if l == "class" => {
                let text = args.text()?;
                class = Some(
                    io_class(&text)
                        .ok_or_else(|| usage(format!("unknown scheduling class: '{text}'")))?,
                );
            }
            Arg::Short('n') => level = Some(args.number()?),
            Arg::Long(l) if l == "classdata" => level = Some(args.number()?),
            Arg::Short('p') => (which, targeted) = (PriorityWhich::Process, true),
            Arg::Long(l) if l == "pid" => (which, targeted) = (PriorityWhich::Process, true),
            Arg::Short('P') => (which, targeted) = (PriorityWhich::Group, true),
            Arg::Long(l) if l == "pgid" => (which, targeted) = (PriorityWhich::Group, true),
            Arg::Short('u') => (which, targeted) = (PriorityWhich::User, true),
            Arg::Long(l) if l == "uid" => (which, targeted) = (PriorityWhich::User, true),
            Arg::Short('t') => tolerant = true,
            Arg::Long(l) if l == "ignore" => tolerant = true,
            Arg::Operand(word) => {
                // After -p, -P or -u the operands name what to change;
                // otherwise they are a command to run.
                let mut operands = vec![word];
                operands.extend(args.rest());
                if targeted {
                    for item in operands {
                        let t = item.to_string_lossy().into_owned();
                        who.push(t.parse().map_err(|_| usage(format!("invalid ID: '{t}'")))?);
                    }
                } else {
                    command = operands;
                }
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if let Some(n) = level
        && n >= abi::IO_LEVELS
    {
        return Err(usage(format!("the level must be 0 to {}", abi::IO_LEVELS - 1)));
    }
    let set = class.is_some() || level.is_some();
    let class = class.unwrap_or(if level.is_some() { IoClass::BestEffort } else { IoClass::None });
    let level = level.unwrap_or(if class == IoClass::BestEffort || class == IoClass::Realtime {
        4
    } else {
        0
    });
    if !command.is_empty() {
        if let Err(status) =
            abi::io_priority(&SYS, SLOT_PROCESS, PriorityWhich::Process, 0, true, class, level)
            && !tolerant
        {
            return Err(fail(format!(
                "ioprio_set failed: {}",
                util::strip_os_error(&sys::io_error(status))
            )));
        }
        return Err(exec(&command));
    }
    if who.is_empty() {
        // This process's own.
        who.push(0);
    }
    let mut code = 0;
    for id in &who {
        match abi::io_priority(&SYS, SLOT_PROCESS, which, *id, set, class, level) {
            Ok(old) if !set => {
                let shown = IoClass::from_raw(old.io_class as u32).unwrap_or(IoClass::None);
                // Only the idle class has no level, as util-linux shows it.
                let line = match shown {
                    IoClass::Idle => class_name(shown).to_string(),
                    _ => format!("{}: prio {}", class_name(shown), old.level),
                };
                if who.len() > 1 { println!("{id}: {line}") } else { println!("{line}") }
            }
            Ok(_) => {}
            Err(status) => {
                if !tolerant {
                    eprintln!("ionice: {id}: {}", util::strip_os_error(&sys::io_error(status)));
                    code = 1;
                }
            }
        }
    }
    Ok(code)
}

/// A CPU mask's bytes: bit n % 8 of byte n / 8 for CPU n.
type Mask = Vec<u8>;

/// Parses a hexadecimal mask (`3`, `0xff`, `ff,00000000`).
fn parse_mask(text: &str) -> Option<Mask> {
    let hex: String = text.trim_start_matches("0x").chars().filter(|&c| c != ',').collect();
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut mask = Vec::new();
    let digits: Vec<u8> =
        hex.bytes().rev().map(|c| (c as char).to_digit(16).unwrap_or(0) as u8).collect();
    for pair in digits.chunks(2) {
        mask.push(pair[0] | pair.get(1).map_or(0, |high| high << 4));
    }
    Some(mask)
}

/// Parses a CPU list (`0-3,6,8-14:2`).
fn parse_list(text: &str) -> Option<Mask> {
    let mut mask = Vec::new();
    for item in text.split(',') {
        let (range, stride) = match item.split_once(':') {
            Some((range, stride)) => (range, stride.parse::<usize>().ok()?.max(1)),
            None => (item, 1),
        };
        let (first, last) = match range.split_once('-') {
            Some((a, b)) => (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?),
            None => {
                let n = range.parse::<usize>().ok()?;
                (n, n)
            }
        };
        if last < first || last >= 4096 {
            return None;
        }
        for cpu in (first..=last).step_by(stride) {
            if mask.len() <= cpu / 8 {
                mask.resize(cpu / 8 + 1, 0);
            }
            mask[cpu / 8] |= 1 << (cpu % 8);
        }
    }
    Some(mask)
}

/// A mask in hexadecimal, without leading zeros.
fn mask_hex(mask: &[u8]) -> String {
    let mut text: String = mask.iter().rev().map(|b| format!("{b:02x}")).collect();
    while text.len() > 1 && text.starts_with('0') {
        text.remove(0);
    }
    text
}

/// A mask as a list, ranges joined (`0-3,6`).
fn mask_list(mask: &[u8]) -> String {
    let cpus: Vec<usize> =
        (0..mask.len() * 8).filter(|&n| mask[n / 8] & (1 << (n % 8)) != 0).collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let start = cpus[i];
        let mut end = start;
        while i + 1 < cpus.len() && cpus[i + 1] == end + 1 {
            i += 1;
            end = cpus[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}-{end}") });
        i += 1;
    }
    parts.join(",")
}

/// Thread `thread`'s mask (0: the calling one).
fn affinity(thread: u64) -> Result<Mask, radix_abi::Status> {
    let mut mask = vec![0u8; 512];
    let got = abi::affinity(&SYS, SLOT_PROCESS, thread, &mut mask)?;
    mask.truncate(got.size as usize);
    Ok(mask)
}

/// taskset(1).
pub fn taskset(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut pid_mode = false;
    let mut list = false;
    let mut all_threads = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('p') => pid_mode = true,
            Arg::Long(l) if l == "pid" => pid_mode = true,
            Arg::Short('c') => list = true,
            Arg::Long(l) if l == "cpu-list" => list = true,
            Arg::Short('a') => all_threads = true,
            Arg::Long(l) if l == "all-tasks" => all_threads = true,
            Arg::Operand(first) => {
                let mut rest = vec![first];
                rest.extend(args.rest());
                return taskset_run(rest, pid_mode, list, all_threads);
            }
            other => return Err(args.unknown(&other)),
        }
    }
    Err(usage("usage: taskset [-p] [-c] [MASK|LIST] [PID | COMMAND [ARGUMENT...]]".into()))
}

fn taskset_run(
    operands: Vec<OsString>,
    pid_mode: bool,
    list: bool,
    all: bool,
) -> Result<i32, Fail> {
    let text: Vec<String> = operands.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let parse = |t: &str| -> Result<Mask, Fail> {
        let mask = if list { parse_list(t) } else { parse_mask(t) };
        mask.ok_or_else(|| {
            usage(format!("failed to parse {} '{t}'", if list { "CPU list" } else { "CPU mask" }))
        })
    };
    if !pid_mode {
        if text.len() < 2 {
            return Err(usage(
                "usage: taskset [-p] [-c] [MASK|LIST] [PID | COMMAND [ARGUMENT...]]".into(),
            ));
        }
        let mask = parse(&text[0])?;
        abi::set_affinity(&SYS, SLOT_PROCESS, 0, &mask).map_err(|s| {
            fail(format!("failed to set affinity: {}", util::strip_os_error(&sys::io_error(s))))
        })?;
        return Err(exec(&operands[1..]));
    }
    let (mask, pid) = match text.as_slice() {
        [pid] => (None, pid),
        [mask, pid] => (Some(parse(mask)?), pid),
        _ => return Err(usage("usage: taskset -p [MASK|LIST] PID".into())),
    };
    let pid: u64 = pid.parse().map_err(|_| usage(format!("invalid PID argument: '{pid}'")))?;
    let threads: Vec<u64> = if all {
        let t: Vec<u64> = proc::threads(pid).iter().map(|t| t.number).collect();
        if t.is_empty() { vec![pid] } else { t }
    } else {
        vec![pid]
    };
    let what = if list { "list" } else { "mask" };
    let show = |m: &Mask| if list { mask_list(m) } else { mask_hex(m) };
    for thread in threads {
        let old = affinity(thread).map_err(|s| {
            fail(format!(
                "failed to get pid {thread}'s affinity: {}",
                util::strip_os_error(&sys::io_error(s))
            ))
        })?;
        println!("pid {thread}'s current affinity {what}: {}", show(&old));
        if let Some(mask) = &mask {
            abi::set_affinity(&SYS, SLOT_PROCESS, thread, mask).map_err(|s| {
                fail(format!(
                    "failed to set pid {thread}'s affinity: {}",
                    util::strip_os_error(&sys::io_error(s))
                ))
            })?;
            let new = affinity(thread).unwrap_or_else(|_| mask.clone());
            println!("pid {thread}'s new affinity {what}: {}", show(&new));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_lists() {
        assert_eq!(parse_mask("3"), Some(vec![3]));
        assert_eq!(parse_mask("0x1ff"), Some(vec![0xff, 1]));
        assert_eq!(parse_mask("zz"), None);
        assert_eq!(parse_list("0-3,6"), Some(vec![0x4f]));
        assert_eq!(parse_list("0-6:2"), Some(vec![0x55]));
        assert_eq!(parse_list("9"), Some(vec![0, 2]));
        assert_eq!(parse_list("3-1"), None);
        assert_eq!(mask_hex(&[0x0f, 0]), "f");
        assert_eq!(mask_hex(&[0, 1]), "100");
        assert_eq!(mask_list(&[0x4f]), "0-3,6");
        assert_eq!(mask_list(&[0x55]), "0,2,4,6");
    }

    #[test]
    fn classes() {
        assert_eq!(io_class("idle"), Some(IoClass::Idle));
        assert_eq!(io_class("2"), Some(IoClass::BestEffort));
        assert_eq!(io_class("bogus"), None);
    }
}
