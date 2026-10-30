// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What the tools share: errors, sizes and times as people read them, the
//! terminal, user names, signals, and the C library's regular expressions
//! and patterns.

use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Why a tool stopped: its exit status and what to say, if anything.
#[derive(Debug)]
pub struct Fail {
    pub code: i32,
    pub message: String,
    /// Whether it was a mistake in the command line, whose status is the
    /// tool's own (`main`'s USAGE_TWO).
    pub usage: bool,
}

/// A failure, with exit status 1.
pub fn fail(message: String) -> Fail {
    Fail { code: 1, message, usage: false }
}

/// A failure with exit status `code`.
pub fn fail_with(code: i32, message: String) -> Fail {
    Fail { code, message, usage: false }
}

/// A mistake in the command line: exit status 1, as most Linux tools
/// give, or 2 for those that say so.
pub fn usage(message: String) -> Fail {
    Fail { code: 1, message, usage: true }
}

impl From<io::Error> for Fail {
    fn from(error: io::Error) -> Fail {
        fail(error.to_string())
    }
}

/// `what: error`, as a failure.
// It takes the error as `map_err` hands it over.
#[allow(clippy::needless_pass_by_value)]
pub fn failed(what: &dyn fmt::Display, error: io::Error) -> Fail {
    fail(format!("{what}: {}", strip_os_error(&error)))
}

/// An error's text without Rust's " (os error N)".
pub fn strip_os_error(error: &io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// A size as util-linux's fstrim and journalctl read it: bytes, or with
/// a suffix of K, M, G, T, P (KiB...) or KB, MB, GB, TB, PB (powers of
/// 1000).
pub fn parse_size(text: &str) -> Option<u64> {
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let suffix = suffix.to_ascii_uppercase();
    let power = |base: u64, n: u32| number.checked_mul(base.checked_pow(n)?);
    let (letter, rest) = suffix.split_at(suffix.len().min(1));
    let n = match letter {
        "" => return Some(number),
        "K" => 1,
        "M" => 2,
        "G" => 3,
        "T" => 4,
        "P" => 5,
        _ => return None,
    };
    match rest {
        "" | "IB" => power(1024, n),
        "B" => power(1000, n),
        _ => None,
    }
}

/// `bytes` with a unit, in powers of 1024 (`si`: of 1000), as `ls -h`
/// and `df -h` print them: rounded up, with one decimal below 10 and
/// none above, and carried into the next unit when it rounds up to one
/// (1048575 bytes is 1.0M).
pub fn human(bytes: u64, si: bool) -> String {
    let base: u128 = if si { 1000 } else { 1024 };
    let units = ["", "K", "M", "G", "T", "P", "E"];
    let bytes = u128::from(bytes);
    let (mut unit, mut scale) = (0, 1);
    while bytes >= scale * base && unit + 1 < units.len() {
        scale *= base;
        unit += 1;
    }
    // Rounded up, as coreutils does, so that a size never reads less.
    let tenths = (bytes * 10).div_ceil(scale);
    let whole = bytes.div_ceil(scale);
    if unit == 0 {
        bytes.to_string()
    } else if tenths < 100 {
        format!("{}.{}{}", tenths / 10, tenths % 10, units[unit])
    } else if whole == base && unit + 1 < units.len() {
        // Rounded up to the next unit, it is one of those.
        format!("1.0{}", units[unit + 1])
    } else {
        format!("{whole}{}", units[unit])
    }
}

/// Nanoseconds since the epoch, now.
pub fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Nanoseconds since boot, now, on the monotonic clock the kernel's
/// times are on.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Seconds since the epoch, formatted with strftime(3) in local time.
pub fn local_time(seconds: i64, format: &str) -> String {
    time_text(seconds, format, false)
}

/// Seconds since the epoch, formatted with strftime(3) in UTC, or in
/// local time.
pub fn time_text(seconds: i64, format: &str, utc: bool) -> String {
    let time = seconds as libc::time_t;
    // SAFETY: an all-zero tm is valid; gmtime_r and localtime_r fill it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let filled = unsafe {
        if utc {
            libc::gmtime_r(&raw const time, &raw mut tm)
        } else {
            libc::localtime_r(&raw const time, &raw mut tm)
        }
    };
    if filled.is_null() {
        return seconds.to_string();
    }
    let Ok(format) = CString::new(format) else { return String::new() };
    let mut buffer = [0u8; 128];
    // SAFETY: the buffer is valid for its length, the format a C string.
    let len = unsafe {
        libc::strftime(buffer.as_mut_ptr().cast(), buffer.len(), format.as_ptr(), &raw const tm)
    };
    String::from_utf8_lossy(&buffer[..len]).into_owned()
}

/// A duration in seconds, as ps(1)'s TIME: `[DD-]HH:MM:SS`.
pub fn clock_time(seconds: u64) -> String {
    let (days, rest) = (seconds / 86400, seconds % 86400);
    let (h, m, s) = (rest / 3600, rest % 3600 / 60, rest % 60);
    if days > 0 { format!("{days}-{h:02}:{m:02}:{s:02}") } else { format!("{h:02}:{m:02}:{s:02}") }
}

/// A duration in seconds, as ps(1)'s ELAPSED: `[[DD-]HH:]MM:SS`.
pub fn elapsed(seconds: u64) -> String {
    let (days, rest) = (seconds / 86400, seconds % 86400);
    let (h, m, s) = (rest / 3600, rest % 3600 / 60, rest % 60);
    if days > 0 {
        format!("{days}-{h:02}:{m:02}:{s:02}")
    } else if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// How long the system has been up, as uptime(1) says it: `5 min`,
/// `2:03`, `3 days, 2:03`.
pub fn up_for(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = seconds % 86400 / 3600;
    let minutes = seconds % 3600 / 60;
    let mut text = String::new();
    if days > 0 {
        text += &format!("{days} day{}, ", if days == 1 { "" } else { "s" });
    }
    if hours > 0 {
        text += &format!("{hours:2}:{minutes:02}");
    } else {
        text += &format!("{minutes} min");
    }
    text
}

/// The terminal's size on `fd`, in columns and rows, if it is one;
/// otherwise COLUMNS and LINES.
pub fn terminal_size(fd: i32) -> Option<(usize, usize)> {
    // SAFETY: an all-zero winsize is valid; the ioctl fills it.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes a winsize to the pointer, which is valid.
    let got = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    if got && size.ws_col > 0 {
        return Some((size.ws_col as usize, size.ws_row as usize));
    }
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
    Some((var("COLUMNS")?, var("LINES").unwrap_or(24)))
}

/// How many columns `text` takes on a terminal: two for each wide East
/// Asian character and emoji, none for combining marks and control
/// characters, one for the rest.
pub fn width(text: &str) -> usize {
    text.chars()
        .map(|c| match c as u32 {
            0..=0x1f | 0x7f..=0x9f | 0x300..=0x36f | 0x200b..=0x200f | 0xfe00..=0xfe0f => 0,
            0x1100..=0x115f
            | 0x2e80..=0x303e
            | 0x3041..=0x33ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa000..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe30..=0xfe4f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1f64f
            | 0x1f900..=0x1f9ff
            | 0x20000..=0x3fffd => 2,
            _ => 1,
        })
        .sum()
}

/// Whether `fd` is a terminal.
pub fn is_terminal(fd: i32) -> bool {
    // SAFETY: isatty reads nothing of the program's.
    unsafe { libc::isatty(fd) == 1 }
}

/// Where the devices are (ADR 0113): the terminals' names are beneath
/// it, as `/devices/pts/0`.
pub const DEVICES: &str = "/devices";

/// A terminal's path as the tools show it, beneath the devices: `pts/0`
/// for `/devices/pts/0`, or for `/dev/pts/0`, through the link that has
/// Linux's name.
pub fn terminal_name(path: &str) -> &str {
    path.strip_prefix("/devices/").or_else(|| path.strip_prefix("/dev/")).unwrap_or(path)
}

/// Whether the locale's characters are UTF-8, as the first of
/// `LC_ALL`, `LC_CTYPE` and `LANG` that is set says (`C.UTF-8`,
/// `en_US.utf8`); with none, the C locale is ASCII.
pub fn utf8_locale() -> bool {
    let set = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    set.is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        name.contains("utf-8") || name.contains("utf8")
    })
}

/// uname(2)'s system name, release and machine.
pub fn uname() -> (String, String, String) {
    // SAFETY: all zeros is a valid utsname: arrays of bytes.
    let mut names: libc::utsname = unsafe { zeroed() };
    // SAFETY: uname fills the structure it is given.
    if unsafe { libc::uname(&raw mut names) } != 0 {
        return ("Dukovo".into(), "unknown".into(), "x86_64".into());
    }
    let text = |field: &[libc::c_char]| {
        let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    (text(&names.sysname), text(&names.release), text(&names.machine))
}

/// The machine's name.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its length.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return "localhost".into();
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// What `read` makes of user `uid`'s entry in the user database, if it
/// has one.
fn with_user<T>(uid: u32, read: impl FnOnce(&libc::passwd) -> T) -> Option<T> {
    // SAFETY: all zeros is a valid passwd: numbers, and pointers that stay
    // null until getpwuid_r fills them.
    let mut pw: libc::passwd = unsafe { zeroed() };
    let mut buffer = vec![0u8; 1024];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call; the buffer for its
    // length.
    let e = unsafe {
        libc::getpwuid_r(uid, &raw mut pw, buffer.as_mut_ptr().cast(), buffer.len(), &raw mut found)
    };
    (e == 0 && !found.is_null()).then(|| read(&pw))
}

/// The name of user `uid`, or its number if it has none.
pub fn user_name(uid: u32) -> String {
    with_user(uid, |pw| {
        // SAFETY: getpwuid_r set pw_name to null or to a C string in the
        // buffer, which lives until with_user returns.
        (!pw.pw_name.is_null())
            .then(|| unsafe { CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned())
    })
    .flatten()
    .unwrap_or_else(|| uid.to_string())
}

/// User `uid`'s login group, as the user database gives it.
pub fn login_group(uid: u32) -> Option<u32> {
    with_user(uid, |pw| pw.pw_gid)
}

/// The name of group `gid`, or its number if it has none.
pub fn group_name(gid: u32) -> String {
    // SAFETY: all zeros is a valid group: numbers, and pointers that stay
    // null until getgrgid_r fills them.
    let mut gr: libc::group = unsafe { zeroed() };
    let mut buffer = vec![0u8; 4096];
    let mut found: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as for getpwuid_r in user_name.
    let e = unsafe {
        libc::getgrgid_r(gid, &raw mut gr, buffer.as_mut_ptr().cast(), buffer.len(), &raw mut found)
    };
    if e != 0 || found.is_null() || gr.gr_name.is_null() {
        return gid.to_string();
    }
    // SAFETY: getgrgid_r set gr_name to a C string in the buffer.
    unsafe { CStr::from_ptr(gr.gr_name) }.to_string_lossy().into_owned()
}

/// The user `name` names, by name or number.
pub fn user_id(name: &str) -> Option<u32> {
    if let Ok(uid) = name.parse() {
        return Some(uid);
    }
    let name = CString::new(name).ok()?;
    // SAFETY: getpwnam takes a C string; the pointer it returns is read
    // before any other call could change what it points to.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    // SAFETY: a non-null result points to a passwd.
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

/// The group `name` names, by name or number.
pub fn group_id(name: &str) -> Option<u32> {
    if let Ok(gid) = name.parse() {
        return Some(gid);
    }
    let name = CString::new(name).ok()?;
    // SAFETY: as for getpwnam.
    let gr = unsafe { libc::getgrnam(name.as_ptr()) };
    // SAFETY: a non-null result points to a group.
    (!gr.is_null()).then(|| unsafe { (*gr).gr_gid })
}

/// A C structure with every byte zero, as C code clears one before a
/// call fills it.
///
/// # Safety
///
/// All zeros must be a valid `T`, as it is for structures of numbers,
/// arrays and raw pointers, and is not for references, `NonZero` numbers
/// or most enums.
pub unsafe fn zeroed<T: Copy>() -> T {
    // SAFETY: the caller promises that all zeros is a valid `T`.
    unsafe { std::mem::zeroed() }
}

/// The signals, by number, as kill(1) names them.
pub const SIGNALS: &[(i32, &str)] = &[
    (libc::SIGHUP, "HUP"),
    (libc::SIGINT, "INT"),
    (libc::SIGQUIT, "QUIT"),
    (libc::SIGILL, "ILL"),
    (libc::SIGTRAP, "TRAP"),
    (libc::SIGABRT, "ABRT"),
    (libc::SIGBUS, "BUS"),
    (libc::SIGFPE, "FPE"),
    (libc::SIGKILL, "KILL"),
    (libc::SIGUSR1, "USR1"),
    (libc::SIGSEGV, "SEGV"),
    (libc::SIGUSR2, "USR2"),
    (libc::SIGPIPE, "PIPE"),
    (libc::SIGALRM, "ALRM"),
    (libc::SIGTERM, "TERM"),
    (libc::SIGCHLD, "CHLD"),
    (libc::SIGCONT, "CONT"),
    (libc::SIGSTOP, "STOP"),
    (libc::SIGTSTP, "TSTP"),
    (libc::SIGTTIN, "TTIN"),
    (libc::SIGTTOU, "TTOU"),
    (libc::SIGURG, "URG"),
    (libc::SIGXCPU, "XCPU"),
    (libc::SIGXFSZ, "XFSZ"),
    (libc::SIGVTALRM, "VTALRM"),
    (libc::SIGPROF, "PROF"),
    (libc::SIGWINCH, "WINCH"),
    (libc::SIGIO, "IO"),
    (libc::SIGSYS, "SYS"),
];

/// The signal `name` names: a number, or a name with or without `SIG`,
/// in any case (`IOT`, `POLL` and `CLD` are other names of three).
pub fn signal_number(name: &str) -> Option<i32> {
    if let Ok(number) = name.parse::<i32>() {
        return (0..65).contains(&number).then_some(number);
    }
    let upper = name.to_ascii_uppercase();
    let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
    let bare = match bare {
        "IOT" => "ABRT",
        "POLL" => "IO",
        "CLD" => "CHLD",
        other => other,
    };
    SIGNALS.iter().find(|(_, n)| *n == bare).map(|(number, _)| *number)
}

/// Signal `number`'s name, without `SIG`.
pub fn signal_name(number: i32) -> Option<&'static str> {
    SIGNALS.iter().find(|(n, _)| *n == number).map(|(_, name)| *name)
}

/// A regular expression of the C library's (regex(3)).
pub struct Regex {
    compiled: Box<RegexT>,
}

/// regex_t: room for the C library's, whose size this does not know.
#[repr(C, align(8))]
struct RegexT([u8; 256]);

// SAFETY: these are the C library's functions, as regex.h declares them;
// RegexT has room and alignment for any regex_t.
unsafe extern "C" {
    fn regcomp(regex: *mut RegexT, pattern: *const libc::c_char, flags: libc::c_int)
    -> libc::c_int;
    fn regexec(
        regex: *const RegexT,
        text: *const libc::c_char,
        nmatch: usize,
        matches: *mut [isize; 2],
        flags: libc::c_int,
    ) -> libc::c_int;
    fn regerror(
        code: libc::c_int,
        regex: *const RegexT,
        buffer: *mut libc::c_char,
        size: usize,
    ) -> usize;
    fn regfree(regex: *mut RegexT);
}

/// regcomp(3)'s REG_EXTENDED, REG_ICASE and REG_NOSUB, the same in every
/// C library.
const REG_EXTENDED: libc::c_int = 1;
const REG_ICASE: libc::c_int = 2;
const REG_NOSUB: libc::c_int = 8;

impl Regex {
    /// Compiles an extended regular expression, ignoring case if asked.
    pub fn new(pattern: &str, ignore_case: bool) -> Result<Regex, String> {
        let text = CString::new(pattern).map_err(|_| "a pattern has a NUL in it".to_string())?;
        let mut compiled = Box::new(RegexT([0; 256]));
        let flags = REG_EXTENDED | REG_NOSUB | if ignore_case { REG_ICASE } else { 0 };
        // SAFETY: `compiled` has room for a regex_t, `text` is a C string.
        let code = unsafe { regcomp(&raw mut *compiled, text.as_ptr(), flags) };
        if code != 0 {
            let mut buffer = [0u8; 256];
            // SAFETY: the buffer is valid for its length; regerror only
            // reads `compiled`.
            unsafe {
                regerror(code, &raw const *compiled, buffer.as_mut_ptr().cast(), buffer.len())
            };
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
            return Err(String::from_utf8_lossy(&buffer[..end]).into_owned());
        }
        Ok(Regex { compiled })
    }

    /// Whether it matches somewhere in `text`.
    pub fn is_match(&self, text: &str) -> bool {
        let Ok(text) = CString::new(text) else { return false };
        // SAFETY: the expression was compiled; no matches are asked for.
        unsafe {
            regexec(&raw const *self.compiled, text.as_ptr(), 0, std::ptr::null_mut(), 0) == 0
        }
    }
}

impl Drop for Regex {
    fn drop(&mut self) {
        // SAFETY: it was compiled, and is freed once.
        unsafe { regfree(&raw mut *self.compiled) };
    }
}

/// Whether `name` matches the shell pattern `pattern` (fnmatch(3)).
pub fn fnmatch(pattern: &str, name: &OsStr) -> bool {
    let (Ok(pattern), Ok(name)) = (CString::new(pattern), CString::new(name.as_bytes())) else {
        return false;
    };
    // SAFETY: both are C strings.
    unsafe { libc::fnmatch(pattern.as_ptr(), name.as_ptr(), 0) == 0 }
}

/// Splits a list of the form ps(1) and pgrep(1) take: items separated by
/// commas or blanks.
pub fn list(text: &str) -> impl Iterator<Item = &str> {
    text.split([',', ' ', '\t']).filter(|item| !item.is_empty())
}

/// `bytes` as C would write a string: printable ASCII as it is, the
/// usual escapes, and `\ooo` or `\xhh` for the rest.
pub fn escape(bytes: &[u8], hex: bool) -> String {
    let mut text = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\n' => text.push_str("\\n"),
            b'\t' => text.push_str("\\t"),
            b'\r' => text.push_str("\\r"),
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            0x20..=0x7e => text.push(b as char),
            _ if hex => text.push_str(&format!("\\x{b:02x}")),
            _ => text.push_str(&format!("\\{b:o}")),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_as_coreutils_prints_them() {
        assert_eq!(human(0, false), "0");
        assert_eq!(human(1023, false), "1023");
        assert_eq!(human(1024, false), "1.0K");
        assert_eq!(human(1536, false), "1.5K");
        assert_eq!(human(10 * 1024 * 1024, false), "10M");
        assert_eq!(human(1_000_000, true), "1.0M");
        // As the host's ls -lh and ls -l --si (coreutils 9.4) print them.
        for (bytes, binary, si) in [
            (1025, "1.1K", "1.1K"),
            (1100, "1.1K", "1.1K"),
            (10240, "10K", "11K"),
            (10241, "11K", "11K"),
            (999_999, "977K", "1.0M"),
            (1_047_552, "1023K", "1.1M"),
            (1_047_553, "1.0M", "1.1M"),
            (1_048_575, "1.0M", "1.1M"),
            (1_153_434, "1.2M", "1.2M"),
            (1_073_741_823, "1.0G", "1.1G"),
        ] {
            assert_eq!(human(bytes, false), binary, "{bytes}");
            assert_eq!(human(bytes, true), si, "{bytes}");
        }
        assert_eq!(human(u64::MAX, false), "16E");
        assert_eq!(human(u64::MAX, true), "19E");
    }

    #[test]
    fn durations() {
        assert_eq!(clock_time(3725), "01:02:05");
        assert_eq!(clock_time(90000), "1-01:00:00");
        assert_eq!(elapsed(65), "01:05");
        assert_eq!(elapsed(3725), "01:02:05");
        assert_eq!(up_for(300), "5 min");
        assert_eq!(up_for(7380), " 2:03");
        assert_eq!(up_for(86400 + 60), "1 day, 1 min");
    }

    #[test]
    fn signals_by_name_and_number() {
        assert_eq!(signal_number("TERM"), Some(libc::SIGTERM));
        assert_eq!(signal_number("sigkill"), Some(libc::SIGKILL));
        assert_eq!(signal_number("9"), Some(9));
        assert_eq!(signal_number("CLD"), Some(libc::SIGCHLD));
        assert_eq!(signal_number("NOPE"), None);
        assert_eq!(signal_name(libc::SIGHUP), Some("HUP"));
    }

    #[test]
    fn regular_expressions_and_patterns() {
        let regex = Regex::new("^ss?h(d|-agent)$", false).unwrap();
        assert!(regex.is_match("sshd"));
        assert!(regex.is_match("sh-agent"));
        assert!(!regex.is_match("bash"));
        assert!(Regex::new("(", false).is_err());
        assert!(Regex::new("SSHD", true).unwrap().is_match("sshd"));
        assert!(fnmatch("*.rs", OsStr::new("main.rs")));
        assert!(!fnmatch("*.rs", OsStr::new("main.c")));
    }

    #[test]
    fn escapes() {
        assert_eq!(escape(b"a\n\"\x01", false), "a\\n\\\"\\1");
        assert_eq!(escape(b"\xff", true), "\\xff");
    }
}
