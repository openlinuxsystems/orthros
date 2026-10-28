// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reading the kernel's `/processes` (radix-proc(5)), Linux's `/proc`: its
//! `key: value` files, and the `key=value` lines of a process's `threads`
//! and `files`.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;

/// Where the kernel's `/processes` is.
pub const PROC: &str = "/processes";

/// A process, as its `status` describes it.
#[derive(Clone, Debug, Default)]
pub struct Process {
    pub number: u64,
    pub name: String,
    /// `created`, `running` or `exited` (radix-process(2)'s states).
    pub state: String,
    pub threads: u64,
    pub domain: String,
    pub parent: u64,
    pub group: u64,
    pub session: u64,
    /// Its controlling terminal, as `pts/N`.
    pub terminal: Option<String>,
    /// That terminal's foreground process group, 0 for none.
    pub terminal_group: Option<u64>,
    /// What it is doing: running, sleeping, disk, stopped or zombie.
    pub run: String,
    /// When it was made, in nanoseconds since boot.
    pub started_ns: u64,
    pub user_ns: u64,
    pub system_ns: u64,
    pub policy: String,
    pub priority: i64,
    pub nice: i64,
    pub io_class: String,
    pub io_level: u32,
    pub virtual_kib: u64,
    pub resident_kib: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub traced: bool,
    /// Its working directory, if the kernel knows it.
    pub directory: Option<OsString>,
}

impl Process {
    /// Its CPU time, in nanoseconds.
    pub fn cpu_ns(&self) -> u64 {
        self.user_ns + self.system_ns
    }

    /// Whether it is a zombie: ended, and not reaped.
    pub fn zombie(&self) -> bool {
        self.run == "zombie"
    }

    /// ps(1)'s state letter.
    pub fn state_letter(&self) -> char {
        run_letter(&self.run)
    }
}

/// ps(1)'s state letter for a `run` value.
pub fn run_letter(run: &str) -> char {
    match run {
        "running" => 'R',
        "disk" => 'D',
        "stopped" => 'T',
        "zombie" => 'Z',
        "new" => 'I',
        "dead" => 'X',
        _ => 'S',
    }
}

/// The `key: value` lines of `text`, values as bytes.
fn pairs(text: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    text.split(|&b| b == b'\n').filter_map(|line| {
        let colon = line.windows(2).position(|w| w == b": ")?;
        Some((&line[..colon], &line[colon + 2..]))
    })
}

/// A value with `/processes`' escapes undone: a backslash and three octal
/// digits stand for a byte (`\040` a space), as on Linux.
pub fn unescape(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        let digits =
            value.get(i + 1..i + 4).filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
        match digits {
            Some(d) if value[i] == b'\\' => {
                let n = d.iter().fold(0u32, |n, &b| n * 8 + u32::from(b - b'0'));
                out.push(n as u8);
                i += 4;
            }
            _ => {
                out.push(value[i]);
                i += 1;
            }
        }
    }
    out
}

/// A number at the start of a value (`12 KiB` is 12).
fn number<T: std::str::FromStr + Default>(value: &[u8]) -> T {
    let text = String::from_utf8_lossy(value);
    text.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or_default()
}

/// Parses a process's `status`.
pub fn parse_status(text: &[u8]) -> Process {
    let mut p = Process::default();
    for (key, value) in pairs(text) {
        let text = || String::from_utf8_lossy(value).into_owned();
        match key {
            b"number" => p.number = number(value),
            b"name" => p.name = String::from_utf8_lossy(&unescape(value)).into_owned(),
            b"state" => p.state = text(),
            b"threads" => p.threads = number(value),
            b"domain" => p.domain = text(),
            b"parent" => p.parent = number(value),
            b"group" => p.group = number(value),
            b"session" => p.session = number(value),
            b"terminal" => p.terminal = (value != b"none").then(text),
            b"terminal_group" => p.terminal_group = Some(number(value)),
            b"run" => p.run = text(),
            b"started_ns" => p.started_ns = number(value),
            b"user_ns" => p.user_ns = number(value),
            b"system_ns" => p.system_ns = number(value),
            b"policy" => p.policy = text(),
            b"priority" => p.priority = number(value),
            b"nice" => p.nice = number(value),
            b"io_class" => p.io_class = text(),
            b"io_level" => p.io_level = number(value),
            b"virtual_memory" => p.virtual_kib = number(value),
            b"resident_memory" => p.resident_kib = number(value),
            b"minor_faults" => p.minor_faults = number(value),
            b"major_faults" => p.major_faults = number(value),
            b"read_bytes" => p.read_bytes = number(value),
            b"written_bytes" => p.written_bytes = number(value),
            b"voluntary_switches" => p.voluntary_switches = number(value),
            b"involuntary_switches" => p.involuntary_switches = number(value),
            b"traced" => p.traced = value == b"yes",
            b"directory" => p.directory = Some(OsString::from_vec(unescape(value))),
            _ => {}
        }
    }
    if p.run.is_empty() {
        p.run = if p.state == "exited" { "zombie".into() } else { "sleeping".into() };
    }
    p
}

/// Process `number`.
pub fn process(number: u64) -> io::Result<Process> {
    let text = fs::read(format!("{PROC}/{number}/status"))?;
    Ok(parse_status(&text))
}

/// Every process the kernel holds, by number; those that end while they
/// are read are left out.
pub fn processes() -> io::Result<Vec<Process>> {
    let mut numbers: Vec<u64> = fs::read_dir(PROC)?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    Ok(numbers.into_iter().filter_map(|number| process(number).ok()).collect())
}

/// The arguments process `number`'s program started with.
pub fn cmdline(number: u64) -> Vec<OsString> {
    let Ok(bytes) = fs::read(format!("{PROC}/{number}/cmdline")) else { return Vec::new() };
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(&bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.split(|&b| b == 0).map(|arg| OsString::from_vec(arg.to_vec())).collect()
}

/// The `key=value` fields of a line, the last one's value running to the
/// end of the line (it may hold spaces: a name, a path).
fn fields<'a>(line: &'a [u8], last: &[u8]) -> Vec<(&'a [u8], &'a [u8])> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let Some(eq) = rest.iter().position(|&b| b == b'=') else { break };
        let key = &rest[..eq];
        let after = &rest[eq + 1..];
        if key == last {
            out.push((key, after));
            break;
        }
        let end = after.iter().position(|&b| b == b' ').unwrap_or(after.len());
        out.push((key, &after[..end]));
        rest = after.get(end + 1..).unwrap_or(&[]);
    }
    out
}

/// A thread, as its process's `threads` describes it.
#[derive(Clone, Debug, Default)]
pub struct Thread {
    pub number: u64,
    pub run: String,
    pub cpu: u32,
    pub user_ns: u64,
    pub system_ns: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
    pub policy: String,
    pub priority: i64,
    pub nice: i64,
    /// What it waits for, if it does.
    pub waits: Option<String>,
    pub name: String,
}

/// Parses a process's `threads`.
pub fn parse_threads(text: &[u8]) -> Vec<Thread> {
    let mut threads = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let mut t = Thread::default();
        for (key, value) in fields(line, b"name") {
            let text = || String::from_utf8_lossy(value).into_owned();
            match key {
                b"number" => t.number = number(value),
                b"run" => t.run = text(),
                b"cpu" => t.cpu = number(value),
                b"user_ns" => t.user_ns = number(value),
                b"system_ns" => t.system_ns = number(value),
                b"voluntary_switches" => t.voluntary_switches = number(value),
                b"involuntary_switches" => t.involuntary_switches = number(value),
                b"policy" => t.policy = text(),
                b"priority" => t.priority = number(value),
                b"nice" => t.nice = number(value),
                b"waits" => t.waits = Some(text()),
                b"name" => t.name = String::from_utf8_lossy(&unescape(value)).into_owned(),
                _ => {}
            }
        }
        threads.push(t);
    }
    threads
}

/// Process `number`'s threads.
pub fn threads(number: u64) -> Vec<Thread> {
    fs::read(format!("{PROC}/{number}/threads")).map_or(Vec::new(), |text| parse_threads(&text))
}

/// A capability a process holds, as its `files` describes it.
#[derive(Clone, Debug, Default)]
pub struct Held {
    /// Its file descriptor, if its slot is one's.
    pub fd: Option<u32>,
    /// Its slot, if it is not a descriptor's.
    pub slot: Option<u32>,
    /// A file's kind (file, directory, char, block, pipe, socket, link),
    /// or the kind of object another capability names.
    pub kind: String,
    /// `r`, `w`, `rw` or `-`, for a file or directory.
    pub access: Option<String>,
    pub rights: String,
    pub id: u64,
    pub size: u64,
    pub fs: String,
    pub mount: u64,
    pub offset: Option<u64>,
    /// Whether the path was too long to keep whole: it is its start.
    pub cut: bool,
    pub path: Option<OsString>,
}

impl Held {
    /// Whether it is a file or directory.
    pub fn is_file(&self) -> bool {
        self.access.is_some()
    }
}

/// Parses a process's `files`.
pub fn parse_files(text: &[u8]) -> Vec<Held> {
    let mut held = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let mut h = Held::default();
        for (key, value) in fields(line, b"path") {
            let text = || String::from_utf8_lossy(value).into_owned();
            match key {
                b"fd" => h.fd = Some(number(value)),
                b"slot" => h.slot = Some(number(value)),
                b"kind" => h.kind = text(),
                b"access" => h.access = Some(text()),
                b"rights" => h.rights = text(),
                b"id" => h.id = number(value),
                b"size" => h.size = number(value),
                b"fs" => h.fs = text(),
                b"mount" => h.mount = number(value),
                b"offset" => h.offset = Some(number(value)),
                b"cut" => h.cut = value == b"yes",
                b"path" => h.path = Some(OsString::from_vec(unescape(value))),
                _ => {}
            }
        }
        held.push(h);
    }
    held
}

/// What process `number` holds.
pub fn files(number: u64) -> io::Result<Vec<Held>> {
    Ok(parse_files(&fs::read(format!("{PROC}/{number}/files"))?))
}

/// What a CPU has done, in nanoseconds, and its switches.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuTimes {
    pub user_ns: u64,
    pub system_ns: u64,
    pub idle_ns: u64,
    pub switches: u64,
}

/// `/processes/stat`.
#[derive(Clone, Debug, Default)]
pub struct Stat {
    /// Each online CPU's times, by number.
    pub cpus: Vec<(u32, CpuTimes)>,
    /// Their sums.
    pub all: CpuTimes,
    /// Threads ready to run.
    pub running: u64,
    /// Threads waiting for a disk.
    pub disk_waiting: u64,
    pub disk_reads: u64,
    pub disk_read_sectors: u64,
    pub disk_writes: u64,
    pub disk_written_sectors: u64,
    /// Pages read back from swap, and written to it, since boot.
    pub swap_ins: u64,
    pub swap_outs: u64,
    /// When the system started, in nanoseconds since the epoch.
    pub boot_time_ns: u64,
}

/// Parses `/processes/stat`.
pub fn parse_stat(text: &[u8]) -> Stat {
    let mut stat = Stat::default();
    for (key, value) in pairs(text) {
        if let Some(cpu) = key.strip_prefix(b"cpu").and_then(|n| std::str::from_utf8(n).ok())
            && let Ok(cpu) = cpu.parse()
        {
            let mut times = CpuTimes::default();
            for (key, value) in fields(value, b"") {
                match key {
                    b"user_ns" => times.user_ns = number(value),
                    b"system_ns" => times.system_ns = number(value),
                    b"idle_ns" => times.idle_ns = number(value),
                    b"switches" => times.switches = number(value),
                    _ => {}
                }
            }
            stat.cpus.push((cpu, times));
            continue;
        }
        match key {
            b"user_ns" => stat.all.user_ns = number(value),
            b"system_ns" => stat.all.system_ns = number(value),
            b"idle_ns" => stat.all.idle_ns = number(value),
            b"switches" => stat.all.switches = number(value),
            b"running" => stat.running = number(value),
            b"disk_waiting" => stat.disk_waiting = number(value),
            b"disk_reads" => stat.disk_reads = number(value),
            b"disk_read_sectors" => stat.disk_read_sectors = number(value),
            b"disk_writes" => stat.disk_writes = number(value),
            b"disk_written_sectors" => stat.disk_written_sectors = number(value),
            b"swap_ins" => stat.swap_ins = number(value),
            b"swap_outs" => stat.swap_outs = number(value),
            b"boot_time_ns" => stat.boot_time_ns = number(value),
            _ => {}
        }
    }
    stat
}

/// `/processes/stat`, now.
pub fn stat() -> io::Result<Stat> {
    Ok(parse_stat(&fs::read(format!("{PROC}/stat"))?))
}

/// `/processes/meminfo`: memory in KiB, by key (`total`, `free`,
/// `page_cache` and the other owners).
pub fn meminfo() -> io::Result<Vec<(String, u64)>> {
    let text = fs::read(format!("{PROC}/meminfo"))?;
    Ok(pairs(&text)
        .map(|(key, value)| (String::from_utf8_lossy(key).into_owned(), number(value)))
        .collect())
}

/// Memory as free(1) shows it, in KiB.
#[derive(Clone, Copy, Debug, Default)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    /// The page cache, which the kernel can drop for programs.
    pub cache: u64,
    /// The swap areas' room, and what of it is free.
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Memory {
    /// What is in use, besides the cache.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free).saturating_sub(self.cache)
    }

    /// What programs can still have: what is free, and the cache.
    pub fn available(&self) -> u64 {
        self.free + self.cache
    }

    /// The swap in use.
    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

/// The system's memory.
pub fn memory() -> io::Result<Memory> {
    let info = meminfo()?;
    let get = |key: &str| info.iter().find(|(k, _)| k == key).map_or(0, |(_, v)| *v);
    Ok(Memory {
        total: get("total"),
        free: get("free"),
        cache: get("page_cache"),
        swap_total: get("swap_total"),
        swap_free: get("swap_free"),
    })
}

/// The load averages over 1, 5 and 15 minutes.
pub fn loadavg() -> [f64; 3] {
    let Ok(text) = fs::read(format!("{PROC}/loadavg")) else { return [0.0; 3] };
    let mut averages = [0.0; 3];
    for (key, value) in pairs(&text) {
        let slot = match key {
            b"load_1" => 0,
            b"load_5" => 1,
            b"load_15" => 2,
            _ => continue,
        };
        averages[slot] = String::from_utf8_lossy(value).trim().parse().unwrap_or(0.0);
    }
    averages
}

/// Nanoseconds since boot.
pub fn uptime_ns() -> u64 {
    let Ok(text) = fs::read(format!("{PROC}/uptime")) else { return 0 };
    pairs(&text).find(|(key, _)| *key == b"uptime_ns").map_or(0, |(_, value)| number(value))
}

/// A mount, as `/processes/mounts` lists it.
#[derive(Clone, Debug)]
pub struct Mount {
    pub source: String,
    pub path: OsString,
    pub kind: String,
}

/// The mounts.
pub fn mounts() -> io::Result<Vec<Mount>> {
    let text = fs::read(format!("{PROC}/mounts"))?;
    let mut mounts = Vec::new();
    for line in text.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
        let [source, path, kind, ..] = parts.as_slice() else { continue };
        mounts.push(Mount {
            source: String::from_utf8_lossy(&unescape(source)).into_owned(),
            path: OsString::from_vec(unescape(path)),
            kind: String::from_utf8_lossy(kind).into_owned(),
        });
    }
    Ok(mounts)
}

/// The number of the process that runs this program.
pub fn own_number() -> u64 {
    std::process::id() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        let p = parse_status(
            b"number: 12\nname: nukesh\nstate: running\nthreads: 1\nparent: 1\ngroup: 12\n\
              session: 12\nterminal: pts/0\nterminal_group: 12\nrun: sleeping\nstarted_ns: 5000\nuser_ns: 10\n\
              system_ns: 20\nnice: -5\nresident_memory: 1024 KiB\ntraced: no\n\
              directory: /home/a b\n",
        );
        assert_eq!(p.number, 12);
        assert_eq!(p.name, "nukesh");
        assert_eq!(p.terminal.as_deref(), Some("pts/0"));
        assert_eq!(p.terminal_group, Some(12));
        assert_eq!(p.cpu_ns(), 30);
        assert_eq!(p.nice, -5);
        assert_eq!(p.resident_kib, 1024);
        assert_eq!(p.directory, Some(OsString::from("/home/a b")));
        let escaped = parse_status(b"name: a\\012b\ndirectory: /x\\134\\012y\n");
        assert_eq!(escaped.name, "a\nb");
        assert_eq!(escaped.directory, Some(OsString::from("/x\\\ny")));
        assert_eq!(p.state_letter(), 'S');
        assert!(parse_status(b"terminal: none\n").terminal.is_none());
    }

    #[test]
    fn thread_and_file_lines() {
        let t = parse_threads(
            b"number=12 run=sleeping cpu=1 user_ns=5 system_ns=6 voluntary_switches=7 \
              involuntary_switches=8 policy=other priority=0 nice=0 waits=pipe_read name=a b\n",
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].cpu, 1);
        assert_eq!(t[0].waits.as_deref(), Some("pipe_read"));
        assert_eq!(t[0].name, "a b");
        let f = parse_files(
            b"slot=4 kind=process rights=read+write\n\
              fd=1 kind=file access=w id=7 size=10 fs=tmpfs mount=2 rights=write offset=3 \
              path=/tmp/x y\n",
        );
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].slot, Some(4));
        assert!(!f[0].is_file());
        assert_eq!(f[1].fd, Some(1));
        assert_eq!(f[1].offset, Some(3));
        assert_eq!(f[1].path, Some(OsString::from("/tmp/x y")));
    }

    #[test]
    fn escapes_are_undone() {
        assert_eq!(unescape(b"a\\040b\\011c\\012\\134"), b"a b\tc\n\\");
        // Not an escape: left as it is.
        assert_eq!(unescape(b"\\9x\\04"), b"\\9x\\04");
    }

    #[test]
    fn stat_lines() {
        let s = parse_stat(
            b"cpu0: user_ns=1 system_ns=2 idle_ns=3 switches=4\ncpu1: user_ns=5 system_ns=6 \
              idle_ns=7 switches=8\ncpus: 2\nuser_ns: 6\nsystem_ns: 8\nidle_ns: 10\n\
              switches: 12\nrunning: 1\ndisk_reads: 9\nboot_time_ns: 100\n",
        );
        assert_eq!(s.cpus.len(), 2);
        assert_eq!(s.cpus[1].1.idle_ns, 7);
        assert_eq!((s.all.user_ns, s.all.system_ns, s.all.idle_ns), (6, 8, 10));
        assert_eq!(s.disk_reads, 9);
        assert_eq!(s.boot_time_ns, 100);
    }
}
