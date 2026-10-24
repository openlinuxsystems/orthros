// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The system calls the shell makes, through the C library, with errors
//! as [`std::io::Error`].
//!
//! The shell forks, so it uses descriptors directly rather than through
//! `std::fs::File`, whose buffers and drops would follow the child.

use std::ffi::{CStr, CString};
use std::io::{Error, Result};

/// Descriptors the shell keeps for itself (saved descriptors, the script
/// it reads) are at or above this, out of the way of the 0 to 9 that
/// redirections name.
pub const HIGH_FD: i32 = 10;

fn check(result: libc::c_int) -> Result<libc::c_int> {
    if result < 0 { Err(Error::last_os_error()) } else { Ok(result) }
}

/// The text of an error, without Rust's "(os error N)".
pub fn describe(error: &Error) -> String {
    match error.raw_os_error() {
        Some(code) => {
            // SAFETY: strerror returns a pointer to a NUL-terminated string
            // that stays valid until the next call; it is copied at once.
            let text = unsafe { CStr::from_ptr(libc::strerror(code)) };
            text.to_string_lossy().into_owned()
        }
        None => error.to_string(),
    }
}

pub fn errno() -> i32 {
    Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Forks: the child's number in the parent, 0 in the child.
pub fn fork() -> Result<i32> {
    // SAFETY: the shell has one thread, so the child gets a consistent
    // copy of its memory.
    check(unsafe { libc::fork() })
}

/// Runs `path` in place of this process; it returns only on failure.
pub fn execve(path: &CStr, argv: &[CString], envp: &[CString]) -> Error {
    let mut args: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    args.push(std::ptr::null());
    let mut env: Vec<*const libc::c_char> = envp.iter().map(|e| e.as_ptr()).collect();
    env.push(std::ptr::null());
    // SAFETY: both arrays end with a null pointer and point at C strings
    // that live until the call returns.
    unsafe { libc::execve(path.as_ptr(), args.as_ptr(), env.as_ptr()) };
    Error::last_os_error()
}

/// A pipe, both ends closed on exec: (read end, write end).
pub fn pipe() -> Result<(i32, i32)> {
    let mut fds = [0; 2];
    // SAFETY: fds has room for the two descriptors pipe2 writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok((fds[0], fds[1]))
}

pub fn dup2(from: i32, to: i32) -> Result<()> {
    if from == to {
        // dup2 would keep close-on-exec; a redirection wants it off.
        return set_cloexec(to, false);
    }
    // SAFETY: dup2 only acts on the descriptor table.
    check(unsafe { libc::dup2(from, to) }).map(drop)
}

/// A copy of `fd` at or above `min`, closed on exec.
pub fn dup_at_least(fd: i32, min: i32) -> Result<i32> {
    // SAFETY: F_DUPFD_CLOEXEC only acts on the descriptor table.
    check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min) })
}

/// A copy of `fd` at or above [`HIGH_FD`], closed on exec.
pub fn dup_high(fd: i32) -> Result<i32> {
    // SAFETY: F_DUPFD_CLOEXEC only acts on the descriptor table.
    check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, HIGH_FD) })
}

pub fn close(fd: i32) {
    // SAFETY: closing a descriptor the shell owns.
    unsafe { libc::close(fd) };
}

pub fn is_open(fd: i32) -> bool {
    // SAFETY: F_GETFD only reads the descriptor's flags.
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

pub fn set_cloexec(fd: i32, on: bool) -> Result<()> {
    // SAFETY: F_GETFD and F_SETFD only act on the descriptor's flags.
    let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
    let flags = if on { flags | libc::FD_CLOEXEC } else { flags & !libc::FD_CLOEXEC };
    // SAFETY: as above.
    check(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) }).map(drop)
}

pub fn open(path: &str, flags: i32, mode: u32) -> Result<i32> {
    let path = crate::text::cstring(path);
    // SAFETY: path is a C string; open reads it and returns a descriptor.
    check(unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, mode as libc::c_uint) })
}

pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize> {
    loop {
        // SAFETY: buf is valid for writes of its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) || crate::trap::pending() {
            return Err(error);
        }
    }
}

/// Reads to the end of `fd`.
pub fn read_all(fd: i32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read(fd, &mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => {}
            Err(error) => return Err(error),
        }
    }
}

pub fn write_all(fd: i32, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        // SAFETY: bytes is valid for reads of its length.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let error = Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

/// Waits for a child: its number and status, or `None` with `WNOHANG`
/// when none has changed.
pub fn waitpid(pid: i32, flags: i32) -> Result<Option<(i32, i32)>> {
    let mut status = 0;
    // SAFETY: status is a valid place for the status.
    let got = unsafe { libc::waitpid(pid, &mut status, flags) };
    match got {
        0 => Ok(None),
        n if n > 0 => Ok(Some((n, status))),
        _ => Err(Error::last_os_error()),
    }
}

pub fn getpid() -> i32 {
    // SAFETY: getpid has no preconditions.
    unsafe { libc::getpid() }
}

pub fn getppid() -> i32 {
    // SAFETY: getppid has no preconditions.
    unsafe { libc::getppid() }
}

pub fn getuid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

pub fn geteuid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub fn getgid() -> u32 {
    // SAFETY: getgid has no preconditions.
    unsafe { libc::getgid() }
}

pub fn setpgid(pid: i32, pgid: i32) -> Result<()> {
    // SAFETY: setpgid only changes process groups.
    check(unsafe { libc::setpgid(pid, pgid) }).map(drop)
}

pub fn getpgrp() -> i32 {
    // SAFETY: getpgrp has no preconditions.
    unsafe { libc::getpgrp() }
}

pub fn tcgetpgrp(fd: i32) -> Result<i32> {
    // SAFETY: tcgetpgrp only reads the terminal's foreground group.
    check(unsafe { libc::tcgetpgrp(fd) })
}

/// Gives the terminal on `fd` to process group `pgid`. From a group in
/// the background the change raises SIGTTOU, which would stop a script's
/// shell taking the terminal back, or a job about to have it; with the
/// signal blocked meanwhile, the change goes ahead, as in bash.
pub fn tcsetpgrp(fd: i32, pgid: i32) -> Result<()> {
    // SAFETY: the sets are filled in before they are used, the old mask
    // comes back, and tcsetpgrp only changes the terminal's foreground
    // group.
    unsafe {
        let mut ttou: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut ttou);
        libc::sigaddset(&mut ttou, libc::SIGTTOU);
        libc::sigprocmask(libc::SIG_BLOCK, &ttou, &mut old);
        let result = check(libc::tcsetpgrp(fd, pgid)).map(drop);
        libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        result
    }
}

pub fn isatty(fd: i32) -> bool {
    // SAFETY: isatty only asks about the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

pub fn kill(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: kill sends a signal and touches no memory.
    check(unsafe { libc::kill(pid, signal) }).map(drop)
}

pub fn umask(mask: u32) -> u32 {
    // SAFETY: umask only sets the process's mask.
    unsafe { libc::umask(mask as libc::mode_t) as u32 }
}

pub fn tcgetattr(fd: i32) -> Option<libc::termios> {
    // SAFETY: termios is plain data; tcgetattr fills it in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: termios is a valid place to fill.
    (unsafe { libc::tcgetattr(fd, &mut termios) } == 0).then_some(termios)
}

pub fn tcsetattr(fd: i32, termios: &libc::termios) {
    // SAFETY: termios is a valid struct from tcgetattr.
    unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, termios) };
}

/// The terminal's size in columns and rows.
pub fn window_size(fd: i32) -> Option<(u16, u16)> {
    // SAFETY: winsize is plain data; TIOCGWINSZ fills it in.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: size is a valid place for TIOCGWINSZ to write.
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some((size.ws_col, size.ws_row))
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf has room for the name gethostname writes.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    crate::text::decode(&buf[..end])
}

/// The home directory of user `name` (`None` for the current user).
pub fn home_of(name: Option<&str>) -> Option<String> {
    // SAFETY: getpwnam and getpwuid return a pointer to a static entry or
    // null; its fields are copied before any other call.
    unsafe {
        let entry = match name {
            Some(name) => {
                let name = crate::text::cstring(name);
                libc::getpwnam(name.as_ptr())
            }
            None => libc::getpwuid(libc::getuid()),
        };
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return None;
        }
        Some(crate::text::decode(CStr::from_ptr((*entry).pw_dir).to_bytes()))
    }
}

/// The login name of the current user.
pub fn user_name() -> Option<String> {
    // SAFETY: as in home_of.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            return None;
        }
        Some(crate::text::decode(CStr::from_ptr((*entry).pw_name).to_bytes()))
    }
}

/// Every user's login name, for completion.
pub fn user_names() -> Vec<String> {
    let mut names = Vec::new();
    // SAFETY: setpwent, getpwent and endpwent walk the user database; each
    // entry's name is copied before the next call.
    unsafe {
        libc::setpwent();
        loop {
            let entry = libc::getpwent();
            if entry.is_null() {
                break;
            }
            if !(*entry).pw_name.is_null() {
                names.push(crate::text::decode(CStr::from_ptr((*entry).pw_name).to_bytes()));
            }
        }
        libc::endpwent();
    }
    names
}

pub type Rlimit = libc::rlimit;

pub fn getrlimit(resource: i32) -> Result<Rlimit> {
    // SAFETY: rlimit is plain data that getrlimit fills in.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: limit is a valid place to fill.
    check(unsafe { libc::getrlimit(resource as _, &mut limit) })?;
    Ok(limit)
}

pub fn setrlimit(resource: i32, limit: &Rlimit) -> Result<()> {
    // SAFETY: limit is a valid rlimit.
    check(unsafe { libc::setrlimit(resource as _, limit) }).map(drop)
}

/// The CPU time used by the shell and by its waited-for children, user
/// and system, in seconds.
pub fn times() -> (f64, f64, f64, f64) {
    let usage = |who| {
        // SAFETY: rusage is plain data that getrusage fills in.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: usage is a valid place to fill.
        unsafe { libc::getrusage(who, &mut usage) };
        let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        (secs(usage.ru_utime), secs(usage.ru_stime))
    };
    let (user, system) = usage(libc::RUSAGE_SELF);
    let (child_user, child_system) = usage(libc::RUSAGE_CHILDREN);
    (user, system, child_user, child_system)
}

/// Whether `path` can be run by this user.
pub fn executable(path: &str) -> bool {
    let c = crate::text::cstring(path);
    // SAFETY: c is a C string that access only reads.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

pub fn access(path: &str, mode: i32) -> bool {
    let c = crate::text::cstring(path);
    // SAFETY: as in executable.
    unsafe { libc::access(c.as_ptr(), mode) == 0 }
}

/// Random bytes from the kernel.
pub fn random_u32() -> u32 {
    let mut value = [0u8; 4];
    // SAFETY: value has room for the 4 bytes asked for.
    let got = unsafe { libc::getrandom(value.as_mut_ptr().cast(), 4, 0) };
    if got == 4 {
        u32::from_ne_bytes(value)
    } else {
        // SAFETY: time with a null pointer only returns the time.
        (unsafe { libc::time(std::ptr::null_mut()) } as u32) ^ (getpid() as u32).rotate_left(16)
    }
}

/// Exits at once, without running Rust's exit handlers or flushing
/// buffers the child copied from its parent.
pub fn exit_now(status: i32) -> ! {
    // SAFETY: _exit ends the process.
    unsafe { libc::_exit(status) }
}

/// The time of day, formatted with strftime's `format`.
pub fn strftime(format: &str, time: i64) -> String {
    let mut out = vec![0u8; 512];
    let c = crate::text::cstring(format);
    // SAFETY: tm is plain data that localtime_r fills in; strftime writes
    // at most out.len() bytes.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = time as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        let n = libc::strftime(out.as_mut_ptr().cast(), out.len(), c.as_ptr(), &tm);
        out.truncate(n);
    }
    crate::text::decode(&out)
}

pub fn now() -> (i64, u32) {
    let since =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (since.as_secs() as i64, since.subsec_micros())
}

/// Moves `fd`'s offset; the new offset.
pub fn lseek(fd: i32, offset: i64, whence: i32) -> Result<i64> {
    // SAFETY: lseek only acts on the descriptor.
    let got = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
    if got < 0 { Err(Error::last_os_error()) } else { Ok(got as i64) }
}

/// Whether `fd` is a regular file, whose offset can be moved back over
/// what was read but not used.
pub fn is_regular(fd: i32) -> bool {
    // SAFETY: stat is plain data that fstat fills in.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFREG
    }
}

/// `fd`'s device, inode and whether it is a regular file.
pub fn identity(fd: i32) -> Option<(u64, u64, bool)> {
    // SAFETY: stat is plain data that fstat fills in.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return None;
        }
        Some((st.st_dev as u64, st.st_ino as u64, st.st_mode & libc::S_IFMT == libc::S_IFREG))
    }
}

/// Waits up to `millis` (forever if negative) for `fd` to have input:
/// whether it has.
pub fn poll_in(fd: i32, millis: i32) -> Result<bool> {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: pfd is one valid pollfd.
    let n = unsafe { libc::poll(&mut pfd, 1, millis) };
    if n < 0 { Err(Error::last_os_error()) } else { Ok(n > 0) }
}
