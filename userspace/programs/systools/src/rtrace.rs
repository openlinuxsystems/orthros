// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! rtrace(1): traces the system calls and capability messages of a
//! process, as strace(1) does on Linux. It runs a command and traces it,
//! or traces running processes (`-p`), through a trace of the kernel's
//! (radix-trace(2)): it reads the trace's records and prints a line for
//! each call, signal and other event, with the call's interface and
//! method by name, its arguments as its interface describes them
//! (`radix_abi::catalog`) and the data of its byte arguments.

use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use radix_abi::catalog::{self, Call, Shape};
use radix_abi::process as abi;
use radix_abi::trace::{
    TRACE_BUFFER, TRACE_DATA_MAX, TraceEvent, TraceFlags, TraceRecord, TraceRecordFlags,
    TraceSignal,
};
use radix_abi::{Status, syscalls};

use crate::args::{Arg, Args};
use crate::proc;
use crate::sys::{self, SLOT_PROCESS, SYS};
use crate::util::{self, Fail, fail, usage};

/// How times are printed before each line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stamp {
    None,
    Seconds,
    Micros,
    Epoch,
    Relative,
}

/// How rtrace runs.
struct Settings {
    follow: bool,
    stamp: Stamp,
    durations: bool,
    count: bool,
    count_too: bool,
    string_max: usize,
    hex: u8,
    quiet: bool,
    only: Vec<String>,
    prefix: bool,
}

/// A record, read from the trace.
struct Record {
    event: u16,
    flags: TraceRecordFlags,
    process: u64,
    thread: u64,
    time_ns: u64,
    duration_ns: u64,
    number: u32,
    label: u32,
    status: u64,
    value: u64,
    args: [u64; 10],
    results: [u64; 10],
    data_in: Vec<u8>,
    data_out: Vec<u8>,
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]))
}

/// Decodes a whole record (`bytes` is `size` long).
fn decode(bytes: &[u8]) -> Option<Record> {
    if bytes.len() < TraceRecord::SIZE {
        return None;
    }
    let words = |at: usize| -> [u64; 10] { std::array::from_fn(|i| u64_at(bytes, at + 8 * i)) };
    let data_in = u32_at(bytes, TraceRecord::OFFSET_DATA_IN) as usize;
    let data_out = u32_at(bytes, TraceRecord::OFFSET_DATA_OUT) as usize;
    let start = TraceRecord::SIZE;
    let end_in = (start + data_in).min(bytes.len());
    let end_out = (end_in + data_out).min(bytes.len());
    Some(Record {
        event: u16::from_le_bytes([
            bytes[TraceRecord::OFFSET_EVENT],
            bytes[TraceRecord::OFFSET_EVENT + 1],
        ]),
        flags: TraceRecordFlags(u16::from_le_bytes([
            bytes[TraceRecord::OFFSET_FLAGS],
            bytes[TraceRecord::OFFSET_FLAGS + 1],
        ]) as u32),
        process: u64_at(bytes, TraceRecord::OFFSET_PROCESS),
        thread: u64_at(bytes, TraceRecord::OFFSET_THREAD),
        time_ns: u64_at(bytes, TraceRecord::OFFSET_TIME_NS),
        duration_ns: u64_at(bytes, TraceRecord::OFFSET_DURATION_NS),
        number: u32_at(bytes, TraceRecord::OFFSET_NUMBER),
        label: u32_at(bytes, TraceRecord::OFFSET_LABEL),
        status: u64_at(bytes, TraceRecord::OFFSET_STATUS),
        value: u64_at(bytes, TraceRecord::OFFSET_VALUE),
        args: words(TraceRecord::OFFSET_ARGS),
        results: words(TraceRecord::OFFSET_RESULTS),
        data_in: bytes[start..end_in].to_vec(),
        data_out: bytes[end_in..end_out].to_vec(),
    })
}

/// Splits whole records off the front of `pending`.
fn split_records(pending: &mut Vec<u8>) -> Vec<Record> {
    let mut records = Vec::new();
    let mut at = 0;
    while pending.len() - at >= 8 {
        let size = u32_at(pending, at) as usize;
        if size < TraceRecord::SIZE {
            // Not a record: the trace is not what rtrace knows.
            at = pending.len();
            break;
        }
        if pending.len() - at < size {
            break;
        }
        if let Some(record) = decode(&pending[at..at + size]) {
            records.push(record);
        }
        at += size;
    }
    pending.drain(..at);
    records
}

/// What a call is: its interface and description, by its number or label.
fn call_of(r: &Record) -> Option<&'static Call> {
    if r.number as u64 == syscalls::CAP_CALL {
        catalog::method(r.label)
    } else {
        catalog::syscall(r.number as u64)
    }
}

/// A call's name: `interface.method`, or the system call's.
fn call_name(r: &Record) -> String {
    match call_of(r) {
        Some(call) if !call.interface.is_empty() => format!("{}.{}", call.interface, call.name),
        Some(call) => call.name.to_string(),
        None if r.number as u64 == syscalls::CAP_CALL => format!("cap_call<{:#010x}>", r.label),
        None => format!("syscall_{}", r.number),
    }
}

/// Bytes as a quoted string, cut to `max`.
fn quoted(bytes: &[u8], max: usize, cut: bool, hex: u8) -> String {
    let shown = &bytes[..bytes.len().min(max)];
    let text = if hex >= 2 {
        shown.iter().map(|b| format!("\\x{b:02x}")).collect()
    } else {
        util::escape(shown, hex == 1)
    };
    let more = cut || bytes.len() > max;
    format!("\"{text}\"{}", if more { "..." } else { "" })
}

/// The flags of type `ty` set in `value`, by name.
fn flags_text(ty: &str, value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut names = Vec::new();
    let mut left = value;
    for (bit, name) in catalog::flag_names(ty) {
        let mask = 1u64 << bit;
        if value & mask != 0 {
            names.push(name.to_string());
            left &= !mask;
        }
    }
    if left != 0 {
        names.push(format!("{left:#x}"));
    }
    names.join("|")
}

/// A value of `shape` in `word`.
fn value_text(shape: Shape, word: u64) -> String {
    match shape {
        Shape::Unsigned => word.to_string(),
        Shape::Signed => (word as i64).to_string(),
        Shape::Bool => (word != 0).to_string(),
        Shape::Slot if word as u32 == radix_abi::endpoint::NO_SLOT => "none".into(),
        Shape::Slot => sys::slot_name(word as u32),
        Shape::Addr => format!("{word:#x}"),
        Shape::Enum(ty) => {
            catalog::enum_name(ty, word).map_or_else(|| word.to_string(), String::from)
        }
        Shape::Flags(ty) => flags_text(ty, word),
        _ => format!("{word:#x}"),
    }
}

/// A call's arguments.
fn args_text(r: &Record, call: Option<&Call>, s: &Settings) -> String {
    let Some(call) = call else {
        return r.args.iter().map(|w| format!("{w:#x}")).collect::<Vec<_>>().join(", ");
    };
    let method = r.number as u64 == syscalls::CAP_CALL;
    // A method's words follow the capability and the info word.
    let (mut parts, base, params) = if method {
        (vec![sys::slot_name(r.args[0] as u32)], 2usize, call.args)
    } else {
        (Vec::new(), 0usize, call.args)
    };
    let bytes = if method {
        catalog::method_bytes(r.label)
    } else {
        catalog::syscall_bytes(r.number as u64)
    };
    // Where each input argument's data is, in the order they come.
    let mut inputs: HashMap<usize, (usize, usize)> = HashMap::new();
    let mut at = 0;
    for (word, out) in bytes {
        if *out {
            continue;
        }
        let index = base + *word as usize;
        let len = r.args.get(index + 1).copied().unwrap_or(0) as usize;
        let take = len.min(TRACE_DATA_MAX as usize).min(r.data_in.len() - at);
        inputs.insert(index, (at, take));
        at += take;
    }
    let first_out = bytes.iter().find(|(_, out)| *out).map(|(w, _)| base + *w as usize);
    let mut word = base;
    for param in params {
        let text = match param.shape {
            Shape::BytesIn => {
                let len = r.args.get(word + 1).copied().unwrap_or(0);
                match inputs.get(&word) {
                    Some(&(from, take)) if take > 0 || len == 0 => {
                        let cut = (len as usize) > take;
                        format!(
                            "{}, {len}",
                            quoted(&r.data_in[from..from + take], s.string_max, cut, s.hex)
                        )
                    }
                    _ => format!("{:#x}, {len}", r.args.get(word).copied().unwrap_or(0)),
                }
            }
            Shape::BytesOut => {
                let len = r.args.get(word + 1).copied().unwrap_or(0);
                if Some(word) == first_out && r.status == 0 && !r.data_out.is_empty() {
                    let cut = r.flags.contains(TraceRecordFlags::OUTPUT_CUT);
                    format!("{}, {len}", quoted(&r.data_out, s.string_max, cut, s.hex))
                } else {
                    format!("{:#x}, {len}", r.args.get(word).copied().unwrap_or(0))
                }
            }
            Shape::Words(n) => {
                let words: Vec<String> = (0..n)
                    .map(|i| format!("{:#x}", r.args.get(word + i).copied().unwrap_or(0)))
                    .collect();
                format!("[{}]", words.join(", "))
            }
            shape => value_text(shape, r.args.get(word).copied().unwrap_or(0)),
        };
        parts.push(text);
        word += param.shape.words();
        if word >= r.args.len() {
            break;
        }
    }
    parts.join(", ")
}

/// A call's result: its results by name, or its error.
fn result_text(r: &Record, call: Option<&Call>) -> String {
    if r.status != 0 {
        let errno = sys::errno_of(Status(r.status));
        let text = util::strip_os_error(&io::Error::from_raw_os_error(errno));
        return format!("-1 {} ({text})", sys::status_name(r.status));
    }
    let Some(call) = call else { return "0".into() };
    let base = if r.number as u64 == syscalls::CAP_CALL { 2 } else { 0 };
    let results = call.results;
    if results.is_empty() {
        return "0".into();
    }
    let mut parts = Vec::new();
    let mut word = base;
    for param in results {
        if word >= r.results.len() {
            break;
        }
        let text = match param.shape {
            Shape::Words(n) => {
                let words: Vec<String> = (0..n)
                    .map(|i| format!("{:#x}", r.results.get(word + i).copied().unwrap_or(0)))
                    .collect();
                format!("[{}]", words.join(", "))
            }
            shape => value_text(shape, r.results[word]),
        };
        parts.push((param.name, text));
        word += param.shape.words();
    }
    match parts.as_slice() {
        [(_, text)] => text.clone(),
        _ => format!(
            "{{{}}}",
            parts.iter().map(|(n, t)| format!("{n}={t}")).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// A wait status as strace says how a process ended.
fn exit_text(status: u64) -> String {
    let status = status as i32;
    let signal = status & 0x7f;
    if signal == 0 {
        format!("+++ exited with {} +++", (status >> 8) & 0xff)
    } else {
        let name =
            util::signal_name(signal).map_or_else(|| signal.to_string(), |n| format!("SIG{n}"));
        let core = if status & 0x80 != 0 { " (core dumped)" } else { "" };
        format!("+++ killed by {name}{core} +++")
    }
}

/// Totals for `-c`.
#[derive(Default)]
struct Totals {
    calls: u64,
    errors: u64,
    ns: u64,
}

/// Prints what records say.
struct Printer<'a> {
    s: &'a Settings,
    out: Box<dyn Write>,
    boot_ns: u64,
    last_ns: u64,
    totals: HashMap<String, Totals>,
}

impl Printer<'_> {
    fn stamp(&mut self, r: &Record) -> String {
        let wall = self.boot_ns + r.time_ns;
        let text = match self.s.stamp {
            Stamp::None => String::new(),
            Stamp::Seconds => {
                format!("{} ", util::local_time((wall / 1_000_000_000) as i64, "%H:%M:%S"))
            }
            Stamp::Micros => format!(
                "{}.{:06} ",
                util::local_time((wall / 1_000_000_000) as i64, "%H:%M:%S"),
                wall % 1_000_000_000 / 1000
            ),
            Stamp::Epoch => format!("{}.{:06} ", wall / 1_000_000_000, wall % 1_000_000_000 / 1000),
            Stamp::Relative => {
                let since =
                    if self.last_ns == 0 { 0 } else { r.time_ns.saturating_sub(self.last_ns) };
                format!("{:>3}.{:06} ", since / 1_000_000_000, since % 1_000_000_000 / 1000)
            }
        };
        self.last_ns = r.time_ns;
        text
    }

    fn line(&mut self, r: &Record, text: &str) -> io::Result<()> {
        let mut line = String::new();
        if self.s.prefix || r.thread != r.process {
            line.push_str(&format!("[pid {:>5}] ", r.thread));
        }
        line.push_str(&self.stamp(r));
        line.push_str(text);
        writeln!(self.out, "{line}")
    }

    /// Whether `-e trace=` lets a call through.
    fn wanted(&self, name: &str) -> bool {
        if self.s.only.is_empty() {
            return true;
        }
        let method = name.rsplit('.').next().unwrap_or(name);
        let interface = name.split('.').next().unwrap_or(name);
        self.s.only.iter().any(|w| w == name || w == method || w == interface)
    }

    fn record(&mut self, r: &Record) -> io::Result<()> {
        if r.flags.contains(TraceRecordFlags::LOST) && !self.s.count {
            self.line(r, "--- some records were lost here ---")?;
        }
        let Some(event) = TraceEvent::from_raw(r.event) else { return Ok(()) };
        match event {
            TraceEvent::Call => {
                let name = call_name(r);
                if !self.wanted(&name) {
                    return Ok(());
                }
                if self.s.count || self.s.count_too {
                    let t = self.totals.entry(name.clone()).or_default();
                    t.calls += 1;
                    t.ns += r.duration_ns;
                    if r.status != 0 {
                        t.errors += 1;
                    }
                    if self.s.count {
                        return Ok(());
                    }
                }
                let call = call_of(r);
                let mut text =
                    format!("{name}({}) = {}", args_text(r, call, self.s), result_text(r, call));
                if self.s.durations {
                    text.push_str(&format!(
                        " <{}.{:06}>",
                        r.duration_ns / 1_000_000_000,
                        r.duration_ns % 1_000_000_000 / 1000
                    ));
                }
                self.line(r, &text)
            }
            _ if self.s.count => Ok(()),
            TraceEvent::Signal => {
                let name = util::signal_name(r.number as i32)
                    .map_or_else(|| r.number.to_string(), |n| format!("SIG{n}"));
                let what = TraceSignal::from_raw(r.value).map_or("?", |w| w.name());
                self.line(r, &format!("--- {name} ({what}) ---"))
            }
            TraceEvent::Exec => {
                let path = quoted(
                    &r.data_in,
                    4096,
                    r.flags.contains(TraceRecordFlags::INPUT_CUT),
                    self.s.hex,
                );
                self.line(r, &format!("exec({path}) = 0"))
            }
            TraceEvent::Fork => self.line(r, &format!("--- child {} started ---", r.value)),
            TraceEvent::Exit => {
                if self.s.quiet {
                    return Ok(());
                }
                self.line(r, &exit_text(r.value))
            }
            TraceEvent::Attach => Ok(()),
        }
    }

    /// `-c`'s table.
    fn summary(&mut self) -> io::Result<()> {
        let mut rows: Vec<(&String, &Totals)> = self.totals.iter().collect();
        rows.sort_by(|a, b| b.1.ns.cmp(&a.1.ns).then(b.1.calls.cmp(&a.1.calls)).then(a.0.cmp(b.0)));
        let all_ns: u64 = rows.iter().map(|(_, t)| t.ns).sum();
        let all_calls: u64 = rows.iter().map(|(_, t)| t.calls).sum();
        let all_errors: u64 = rows.iter().map(|(_, t)| t.errors).sum();
        let rule = "------ ----------- ----------- --------- --------- ----------------";
        writeln!(self.out, "% time     seconds  usecs/call     calls    errors call")?;
        writeln!(self.out, "{rule}")?;
        for (name, t) in &rows {
            let share = if all_ns == 0 { 0.0 } else { t.ns as f64 * 100.0 / all_ns as f64 };
            let errors = if t.errors > 0 { t.errors.to_string() } else { String::new() };
            writeln!(
                self.out,
                "{share:6.2} {:11.6} {:11} {:9} {errors:>9} {name}",
                t.ns as f64 / 1e9,
                t.ns / t.calls.max(1) / 1000,
                t.calls
            )?;
        }
        writeln!(self.out, "{rule}")?;
        let errors = if all_errors > 0 { all_errors.to_string() } else { String::new() };
        writeln!(
            self.out,
            "100.00 {:11.6} {:11} {all_calls:9} {errors:>9} total",
            all_ns as f64 / 1e9,
            all_ns / all_calls.max(1) / 1000
        )
    }
}

/// Set by SIGINT, SIGTERM or SIGHUP while tracing running processes.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Traces process `pid` into a new file descriptor.
fn attach(pid: u64, flags: TraceFlags) -> Result<File, Status> {
    loop {
        // The lowest free descriptor: the one /dev/null would get.
        // SAFETY: opening and closing a descriptor of our own.
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            return Err(radix_abi::errors::Kernel::NoRoom.status(0));
        }
        // SAFETY: `fd` was just opened here.
        unsafe { libc::close(fd) };
        match abi::trace(&SYS, SLOT_PROCESS, pid, flags, sys::slot_of(fd)) {
            // SAFETY: the kernel put the trace file in descriptor `fd`'s
            // slot, which nothing else owns.
            Ok(()) => return Ok(unsafe { File::from_raw_fd(fd) }),
            Err(status)
                if radix_abi::errors::Kernel::from_status(status)
                    == Some(radix_abi::errors::Kernel::Occupied) => {}
            Err(status) => return Err(status),
        }
    }
}

/// rtrace(1).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut s = Settings {
        follow: false,
        stamp: Stamp::None,
        durations: false,
        count: false,
        count_too: false,
        string_max: 32,
        hex: 0,
        quiet: false,
        only: Vec::new(),
        prefix: false,
    };
    let mut pids: Vec<u64> = Vec::new();
    let mut output: Option<OsString> = None;
    let mut command: Vec<OsString> = Vec::new();
    let mut no_data = false;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('f') => s.follow = true,
            Arg::Short('t') => {
                s.stamp = match s.stamp {
                    Stamp::None => Stamp::Seconds,
                    Stamp::Seconds => Stamp::Micros,
                    _ => Stamp::Epoch,
                }
            }
            Arg::Short('r') => s.stamp = Stamp::Relative,
            Arg::Short('T') => s.durations = true,
            Arg::Short('c') => s.count = true,
            Arg::Short('C') => s.count_too = true,
            Arg::Short('s') => s.string_max = args.number()?,
            Arg::Short('x') => s.hex += 1,
            Arg::Short('q') => s.quiet = true,
            Arg::Short('n') => no_data = true,
            Arg::Short('o') => output = Some(args.value()?),
            Arg::Short('p') => {
                for pid in util::list(&args.text()?) {
                    pids.push(
                        pid.parse().map_err(|_| usage(format!("invalid process id: '{pid}'")))?,
                    );
                }
            }
            Arg::Short('e') => {
                let expr = args.text()?;
                let names = expr.strip_prefix("trace=").unwrap_or(&expr);
                s.only.extend(names.split(',').filter(|n| !n.is_empty()).map(String::from));
            }
            Arg::Short('h') => {
                println!(
                    "usage: rtrace [-cCfqrTtx] [-e trace=NAME,...] [-o FILE] [-s SIZE] {{-p PID | COMMAND [ARGUMENT...]}}"
                );
                return Ok(0);
            }
            Arg::Short('V') => {
                println!("rtrace from Dukovo's systools {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
            Arg::Operand(word) => {
                command.push(word);
                command.extend(args.rest());
            }
            other => return Err(args.unknown(&other)),
        }
    }
    if pids.is_empty() == command.is_empty() {
        return Err(usage("must have PROG [ARGS] or -p PID".into()));
    }
    s.prefix = s.follow || pids.len() > 1;
    let mut flags = if no_data { TraceFlags::NONE } else { TraceFlags::DATA };
    if s.follow {
        flags = flags.union(TraceFlags::CHILDREN);
    }
    let out: Box<dyn Write> = match &output {
        Some(path) => Box::new(io::BufWriter::new(
            File::create(path)
                .map_err(|e| util::failed(&std::path::Path::new(path).display(), e))?,
        )),
        None => Box::new(io::stderr()),
    };
    let boot_ns = proc::stat().map(|s| s.boot_time_ns).unwrap_or(0);
    let mut printer = Printer { s: &s, out, boot_ns, last_ns: 0, totals: HashMap::new() };
    let mut traces: Vec<File> = Vec::new();
    let mut child: Option<libc::pid_t> = None;
    if command.is_empty() {
        for pid in &pids {
            match attach(*pid, flags) {
                Ok(file) => {
                    traces.push(file);
                    if !s.quiet {
                        eprintln!("rtrace: Process {pid} attached");
                    }
                }
                Err(status) => {
                    return Err(fail(format!(
                        "attach: {pid}: {}",
                        util::strip_os_error(&sys::io_error(status))
                    )));
                }
            }
        }
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut action: libc::sigaction = util::unsafe_zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            // SAFETY: the handler only stores to an atomic.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
    } else {
        let (pid, file) = start(&command, flags)?;
        child = Some(pid);
        traces.push(file);
        // The terminal's Ctrl-C is for the command: rtrace reads on to
        // its end.
        // SAFETY: setting dispositions touches no memory.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        }
    }
    read_all(&mut printer, &mut traces)?;
    if STOP.load(Ordering::Relaxed) && !s.quiet {
        for pid in &pids {
            eprintln!("rtrace: Process {pid} detached");
        }
    }
    drop(traces);
    if s.count || s.count_too {
        printer.summary()?;
    }
    printer.out.flush()?;
    let Some(pid) = child else { return Ok(0) };
    let mut status = 0;
    // SAFETY: waits for our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    Ok(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    })
}

/// Starts `command`, traced: forks a child that waits for word to run it,
/// traces the child, and tells it to go on.
fn start(command: &[OsString], flags: TraceFlags) -> Result<(libc::pid_t, File), Fail> {
    let c: Vec<CString> = command.iter().filter_map(|a| CString::new(a.as_bytes()).ok()).collect();
    let mut argv: Vec<*const libc::c_char> = c.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut go = [0; 2];
    // SAFETY: `go` has room for two descriptors.
    if unsafe { libc::pipe(go.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the program is single-threaded; the child only reads,
    // closes, execs and exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    if pid == 0 {
        // SAFETY: async-signal-safe calls on our own descriptors and the
        // argv built before the fork.
        unsafe {
            libc::close(go[1]);
            let mut byte = 0u8;
            libc::read(go[0], (&mut byte as *mut u8).cast(), 1);
            libc::close(go[0]);
            libc::execvp(argv[0], argv.as_ptr());
            let message = b"rtrace: cannot run the command\n";
            libc::write(2, message.as_ptr().cast(), message.len());
            libc::_exit(127);
        }
    }
    // SAFETY: closing our copy of the read end.
    unsafe { libc::close(go[0]) };
    let file = attach(pid as u64, flags);
    // Go on, traced or not: a failure kills it below.
    // SAFETY: writes a byte from a local and closes our descriptor.
    unsafe {
        libc::write(go[1], b"g".as_ptr().cast(), 1);
        libc::close(go[1]);
    }
    match file {
        Ok(file) => Ok((pid, file)),
        Err(status) => {
            // SAFETY: our own child.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            Err(fail(format!(
                "cannot trace the command: {}",
                util::strip_os_error(&sys::io_error(status))
            )))
        }
    }
}

/// Reads every trace to its end, printing as records come.
fn read_all(printer: &mut Printer, traces: &mut [File]) -> Result<(), Fail> {
    use std::os::fd::AsRawFd;
    let mut pending: Vec<Vec<u8>> = vec![Vec::new(); traces.len()];
    let mut open: Vec<bool> = vec![true; traces.len()];
    let mut buffer = vec![0u8; TRACE_BUFFER as usize];
    while open.iter().any(|&o| o) {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        let mut fds: Vec<libc::pollfd> = traces
            .iter()
            .zip(&open)
            .map(|(t, &o)| libc::pollfd {
                fd: if o { t.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is valid for its length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        for (i, fd) in fds.iter().enumerate() {
            if !open[i] || fd.revents == 0 {
                continue;
            }
            match traces[i].read(&mut buffer) {
                Ok(0) => open[i] = false,
                Ok(n) => {
                    pending[i].extend_from_slice(&buffer[..n]);
                    for record in split_records(&mut pending[i]) {
                        printer.record(&record)?;
                    }
                    printer.out.flush()?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(fail(format!("reading the trace: {}", util::strip_os_error(&e))));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record's bytes, as the kernel writes them.
    fn record(event: TraceEvent, number: u32, label: u32, args: [u64; 10], data: &[u8]) -> Vec<u8> {
        let size = (TraceRecord::SIZE + data.len()).next_multiple_of(8);
        let mut b = vec![0u8; size];
        b[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        b[4..6].copy_from_slice(&event.raw().to_le_bytes());
        b[8..16].copy_from_slice(&7u64.to_le_bytes());
        b[16..24].copy_from_slice(&7u64.to_le_bytes());
        b[TraceRecord::OFFSET_NUMBER..][..4].copy_from_slice(&number.to_le_bytes());
        b[TraceRecord::OFFSET_LABEL..][..4].copy_from_slice(&label.to_le_bytes());
        b[TraceRecord::OFFSET_DATA_IN..][..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        for (i, w) in args.iter().enumerate() {
            b[TraceRecord::OFFSET_ARGS + 8 * i..][..8].copy_from_slice(&w.to_le_bytes());
        }
        b[TraceRecord::SIZE..TraceRecord::SIZE + data.len()].copy_from_slice(data);
        b
    }

    #[test]
    fn records_split_and_decode() {
        let mut pending =
            record(TraceEvent::Call, 11, 0, [0, 0x1000, 5, 0, 0, 0, 0, 0, 0, 0], b"hello");
        let second = record(TraceEvent::Exit, 0, 0, [0; 10], b"");
        pending.extend_from_slice(&second[..100]);
        let records = split_records(&mut pending);
        assert_eq!(records.len(), 1);
        assert_eq!(pending.len(), 100);
        let r = &records[0];
        assert_eq!(r.process, 7);
        assert_eq!(r.data_in, b"hello");
        assert_eq!(call_name(r), "debug_log");
        let s = Settings {
            follow: false,
            stamp: Stamp::None,
            durations: false,
            count: false,
            count_too: false,
            string_max: 32,
            hex: 0,
            quiet: false,
            only: Vec::new(),
            prefix: false,
        };
        assert!(
            args_text(r, call_of(r), &s).contains("\"hello\", 5"),
            "{}",
            args_text(r, call_of(r), &s)
        );
        pending.extend_from_slice(&second[100..]);
        assert_eq!(split_records(&mut pending).len(), 1);
    }

    #[test]
    fn methods_by_name() {
        let r = decode(&record(
            TraceEvent::Call,
            0,
            abi::SET_DIRECTORY,
            [65, 0, 0, 2, 0, 0, 0, 0, 0, 0],
            b"/x",
        ))
        .unwrap();
        assert_eq!(call_name(&r), "process.set_directory");
        assert_eq!(exit_text(3 << 8), "+++ exited with 3 +++");
        assert_eq!(exit_text(9), "+++ killed by SIGKILL +++");
        assert_eq!(quoted(b"abcdef", 3, false, 0), "\"abc\"...");
    }
}
