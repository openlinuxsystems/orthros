// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dukovo's native administration tools (ADR 0096): the commands an
//! administrator expects from Linux whose Linux versions read Linux's
//! `/proc`, utmp or system calls, written for Radix, with their names and
//! common options, as one program. A link by each tool's name runs it
//! (`/system/commands/ps` is a link to `systools`), and so does
//! `systools <tool> [argument...]`.
//!
//! The process tools read the kernel's `/processes` (radix-proc(5)); `lsof`
//! lists the capabilities each process holds; `rtrace` reads a trace of
//! a process's system calls (radix-trace(2)), and `dmesg` the kernel log
//! through `/devices/kmsg`; `logd` keeps the system's log and its audit
//! log, which `log` and `journalctl` read and `logtrim` trims (ADRs 0210
//! and 0211), and `logger` writes a line to it; `notifyd` passes
//! notifications from `notify-send` and other programs to the console,
//! and `notifications` lists them (ADR 0212).
//! `netd` sets the network cards up (ADR 0191), and `resolved` answers
//! every program's DNS queries, which `resolvectl` shows (ADR 0193);
//! `timed` keeps the clock right from the network over NTS, and
//! `timedatectl` shows it and sets the time zone (ADR 0194); `netlog`
//! sends the log to another computer as it is written (ADR 0196).
//! `authd` keeps the accounts, which `user` and `passwd` change, `login`
//! signs people in through it, and `elevate`, and `sudo` in front of it,
//! run a command as an administrator (ADRs 0218, 0250 and 0219); `last`,
//! `lastb` and `lastlog` show the sign-ins it records (ADR 0254).
//! `bundle` makes, signs, installs and runs bundles, which `bundled`
//! installs, seals and removes (ADRs 0221 to 0223 and 0225); called by a
//! name that is no tool's, through a link in a commands folder of
//! bundled's, it runs the bundle command of that name.

mod account;
mod args;
mod authd;
mod balance;
mod blkid;
mod bundled;
mod bundles;
mod column;
mod device;
mod df;
mod dhcp;
mod diskhealth;
mod dmesg;
mod ectool;
mod eject;
mod elevate;
mod facl;
mod findmnt;
mod fstrim;
mod hexdump;
mod hwinfo;
mod iostat;
mod ip;
mod keymaps;
mod last;
mod log;
mod logd;
mod logger;
mod login;
mod logtrim;
mod lsblk;
mod lscpu;
mod lsmem;
mod lsof;
mod lspci;
mod lsusb;
mod memory;
mod modes;
mod netd;
mod netlog;
mod notifyd;
mod nvme;
mod pidstat;
mod power;
mod priority;
mod probe;
mod proc;
mod prompt;
mod ps;
mod quota;
mod remount;
mod replace;
mod resolvectl;
mod resolved;
mod rtrace;
mod script;
mod scrub;
mod sensors;
mod servicefiles;
mod sessions;
mod signals;
mod signin;
mod ss;
mod subvolume;
mod swap;
mod sys;
mod timed;
mod timedatectl;
mod tls;
mod top;
mod tree;
mod unlock;
mod util;
mod volumed;
mod watch;
mod watchdog;
mod which;
mod xxd;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use args::Args;
use util::Fail;

/// A tool: it takes its arguments and returns its exit status.
type Tool = fn(&mut Args) -> Result<i32, Fail>;

/// The tools, by name, in order.
const TOOLS: &[(&str, Tool)] = &[
    ("authd", authd::main),
    ("balance", balance::main),
    ("blkid", blkid::main),
    ("bundle", bundles::main),
    ("bundled", bundled::main),
    ("chgrp", modes::chgrp),
    ("chmod", modes::chmod),
    ("chown", modes::chown),
    ("column", column::main),
    ("df", df::main),
    ("device", device::main),
    ("diskhealth", diskhealth::main),
    ("dmesg", dmesg::main),
    ("ectool", ectool::main),
    ("eject", eject::main),
    ("elevate", elevate::main),
    ("findmnt", findmnt::main),
    ("fstrim", fstrim::main),
    ("free", memory::free),
    ("getfacl", facl::getfacl),
    ("halt", power::halt),
    ("hd", hexdump::hd),
    ("hexdump", hexdump::main),
    ("hwinfo", hwinfo::main),
    ("ionice", priority::ionice),
    ("iostat", iostat::main),
    ("ip", ip::main),
    ("journalctl", log::journalctl),
    ("keymap", keymaps::main),
    ("kill", signals::kill),
    ("last", last::last),
    ("lastb", last::lastb),
    ("lastlog", last::lastlog),
    ("log", log::main),
    ("logd", logd::main),
    ("logger", logger::main),
    ("login", login::main),
    ("logtrim", logtrim::main),
    ("lsblk", lsblk::main),
    ("lscpu", lscpu::main),
    ("lsmem", lsmem::main),
    ("lsof", lsof::main),
    ("lspci", lspci::main),
    ("lsusb", lsusb::main),
    ("mkswap", swap::mkswap),
    ("netd", netd::main),
    ("netlog", netlog::main),
    ("notifications", notifyd::notifications),
    ("notify-send", notifyd::notify_send),
    ("notifyd", notifyd::main),
    ("nvme", nvme::main),
    ("passwd", account::passwd),
    ("pgrep", signals::pgrep),
    ("pidstat", pidstat::main),
    ("pkill", signals::pkill),
    ("poweroff", power::poweroff),
    ("ps", ps::main),
    ("quota", quota::main),
    ("reboot", power::reboot),
    ("remount", remount::main),
    ("renice", priority::renice),
    ("replace", replace::main),
    ("resolvectl", resolvectl::main),
    ("resolved", resolved::main),
    ("rtrace", rtrace::main),
    ("script", script::main),
    ("scrub", scrub::main),
    ("sensors", sensors::main),
    ("service", power::service),
    ("setfacl", facl::setfacl),
    ("shutdown", power::shutdown),
    ("ss", ss::main),
    ("subvolume", subvolume::main),
    ("sudo", elevate::main),
    ("swapoff", swap::swapoff),
    ("swapon", swap::swapon),
    ("taskset", priority::taskset),
    ("timed", timed::main),
    ("timedatectl", timedatectl::main),
    ("top", top::main),
    ("tree", tree::main),
    ("unlock", unlock::main),
    ("uptime", sessions::uptime),
    ("user", account::user),
    ("users", sessions::users),
    ("vmstat", memory::vmstat),
    ("volumed", volumed::main),
    ("w", sessions::w),
    ("watch", watch::main),
    ("watchdog", watchdog::main),
    ("which", which::main),
    ("who", sessions::who),
    ("xxd", xxd::main),
];

/// The tools whose Linux versions exit with 2, not 1, for a mistake in
/// the command line: procps-ng's pgrep and pkill, and which.
const USAGE_TWO: &[&str] = &["getfacl", "pgrep", "pkill", "setfacl", "which"];

/// The tool named `name`.
fn tool(name: &str) -> Option<Tool> {
    TOOLS.iter().find(|(tool, _)| *tool == name).map(|(_, run)| *run)
}

fn main() -> ExitCode {
    // SAFETY: setting a signal's disposition to the default touches no
    // memory; it is done before any thread starts.
    unsafe {
        // A closed pipe ends a tool quietly, as it does a C program,
        // where Rust would have every write after it fail.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    let called = argv.first().map(Path::new).and_then(Path::file_name);
    let mut name = called.map_or_default(|name| name.to_string_lossy().into_owned());
    if tool(&name).is_none() && name != "systools" && !name.is_empty() {
        // A bundle's command, through the link bundled made by its name
        // (ADR 0225): everything after it is the command's.
        let code = bundles::run::command(&name, argv.split_off(1));
        return ExitCode::from(u8::try_from(code).unwrap_or(1));
    }
    if tool(&name).is_none() {
        // `systools <tool> ...`, or `systools --list`.
        match argv.get(1).map(|arg| arg.to_string_lossy().into_owned()) {
            Some(arg) if arg == "--list" => {
                let mut out = io::stdout().lock();
                for (tool, _) in TOOLS {
                    let _ = writeln!(out, "{tool}");
                }
                return ExitCode::SUCCESS;
            }
            Some(arg) if tool(&arg).is_some() => {
                argv.remove(0);
                name = arg;
            }
            _ => {
                eprintln!("usage: systools TOOL [ARGUMENT...]   (systools --list names them)");
                return ExitCode::from(2);
            }
        }
    }
    let run = tool(&name).expect("checked above");
    let name: &'static str = TOOLS.iter().find(|(tool, _)| *tool == name).map_or("", |(t, _)| t);
    let mut args = Args::new(name, argv.into_iter().skip(1).collect());
    let code = match run(&mut args) {
        Ok(code) => code,
        Err(fail) => {
            if !fail.message.is_empty() {
                eprintln!("{name}: {}", fail.message);
            }
            if fail.usage && USAGE_TWO.contains(&name) { 2 } else { fail.code }
        }
    };
    // A closed standard output (`ps | head`) is not an error to report.
    let _ = io::stdout().flush();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
