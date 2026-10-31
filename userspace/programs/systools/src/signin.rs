// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! How authd(8) starts a session (ADR 0250): the first process of
//! someone who signed in, acting for them, on the terminal sign-in sent.
//!
//! authd holds the names capability (radix-names(2)), which Spark gives
//! only a service whose file asks for it. With it authd stamps a copy of
//! its root for the user, derives one without the bypass right or the
//! control right from it,
//! makes a process, names it for the user and their groups, and gives
//! it that root, the random source, the log and the terminal as its
//! descriptors 0 to 2. The process runs [`FIRST_PROGRAM`], `login
//! --session`, as the user: it makes the terminal its session's and
//! runs their shell. Nothing of authd's own passes to it: not its root,
//! not the names capability, not the bypass right.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use radix_abi::capability::{self, ObjectKind, Rights};
use radix_abi::directory::{self, OpenFlags};
use radix_abi::{Status, names, process, startup};

use accounts::User;

use crate::sys::{self, SYS};

/// What a session runs first: login(1), which makes the terminal the
/// session's and runs the user's shell.
pub const FIRST_PROGRAM: &str = "/system/commands/login";

/// The slots a session's process has, as a POSIX program Spark starts.
pub const SLOTS: u32 = 128;

/// The environment variable that says which descriptor holds the names
/// capability Spark gave authd (service.toml(5), `names`).
pub const NAMES_DESCRIPTOR: &str = "NAMES_DESCRIPTOR";

/// The slot of the names capability Spark gave authd, if it did.
pub fn names_slot() -> Option<u32> {
    let fd: i32 = std::env::var(NAMES_DESCRIPTOR).ok()?.parse().ok()?;
    let slot = sys::slot_of(fd);
    let info = capability::info(&SYS, slot).ok()?;
    (info.kind == ObjectKind::Names).then_some(slot)
}

/// The most descriptors a request comes with: an `elevate` request's
/// three for the command and its pipe.
pub const DESCRIPTORS_MAX: usize = 4;

/// Reads from `stream` into `buffer`, as read(2) does, and keeps the
/// descriptors sent with the bytes (SCM_RIGHTS) in `fds`, up to
/// [`DESCRIPTORS_MAX`] in all; any more are closed.
pub fn receive(
    stream: &UnixStream,
    buffer: &mut [u8],
    fds: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec { iov_base: buffer.as_mut_ptr().cast(), iov_len: buffer.len() };
    // Room for more descriptors than are kept, aligned for the headers.
    let mut control = [0u64; 8];
    // SAFETY: all zeros is a valid msghdr: numbers and null pointers.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&control) as _;
    // SAFETY: the message points at our buffer and control space.
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &raw mut message, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the control messages are those recvmsg wrote into
    // `control`, walked with the C library's macros; each descriptor in
    // one is ours to own.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&raw const message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header);
                let bytes = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for k in 0..bytes / size_of::<libc::c_int>() {
                    let at = data.add(k * size_of::<libc::c_int>());
                    let received = OwnedFd::from_raw_fd(at.cast::<libc::c_int>().read_unaligned());
                    if fds.len() < DESCRIPTORS_MAX {
                        fds.push(received);
                    }
                }
            }
            header = libc::CMSG_NXTHDR(&raw const message, header);
        }
    }
    Ok(n as usize)
}

/// Sends `bytes` on `stream` with the descriptors `fds` (SCM_RIGHTS) in
/// the first message, as [`receive`] reads them.
pub fn send_with_fds(stream: &UnixStream, bytes: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
    let mut control = [0u64; 8];
    let room = size_of_val(&fds[..fds.len().min(DESCRIPTORS_MAX)]) as u32;
    // SAFETY: all zeros is a valid msghdr: numbers and null pointers.
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &raw mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE only computes a size, which fits `control` for
    // DESCRIPTORS_MAX descriptors.
    message.msg_controllen = unsafe { libc::CMSG_SPACE(room) } as _;
    // SAFETY: the control space holds one header and the descriptors, as
    // CMSG_SPACE said; the macros point into it.
    let sent = unsafe {
        let header = libc::CMSG_FIRSTHDR(&raw const message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(room) as _;
        let data = libc::CMSG_DATA(header);
        for (k, fd) in fds.iter().take(DESCRIPTORS_MAX).enumerate() {
            data.add(k * size_of::<libc::c_int>()).cast::<libc::c_int>().write_unaligned(*fd);
        }
        libc::sendmsg(stream.as_raw_fd(), &raw const message, 0)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    // What the first message did not take goes after it, as a write.
    let rest = &bytes[sent as usize..];
    if rest.is_empty() { Ok(()) } else { (&mut &*stream).write_all(rest) }
}

/// Whether `fd` is a terminal.
pub fn is_terminal(fd: &OwnedFd) -> bool {
    // SAFETY: isatty only looks at the descriptor.
    unsafe { libc::isatty(fd.as_raw_fd()) == 1 }
}

/// The name of the terminal in `fd`, as `pts/3`.
pub fn terminal_name(fd: &OwnedFd) -> String {
    // SAFETY: ttyname returns a C string the C library keeps, or null;
    // authd has one thread.
    let name = unsafe { libc::ttyname(fd.as_raw_fd()) };
    if name.is_null() {
        return "a terminal".into();
    }
    // SAFETY: a non-null result is a C string.
    let path = unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned();
    crate::util::terminal_name(&path).to_string()
}

/// `term` if it is a terminal type a session may be given, `dumb` if not:
/// letters, digits, `-`, `_`, `.` and `+`, at most 64.
pub fn terminal_type(term: &str) -> &str {
    let fine = |c: char| c.is_ascii_alphanumeric() || "-_.+".contains(c);
    if !term.is_empty() && term.len() <= 64 && term.chars().all(fine) { term } else { "dumb" }
}

/// The environment a session starts with: what login(1) sets on Linux,
/// over what Spark gives every POSIX program.
pub fn environment(user: &User, term: &str) -> Vec<String> {
    vec![
        format!("HOME={}", user.home),
        format!("SHELL={}", user.shell),
        format!("USER={}", user.name),
        format!("LOGNAME={}", user.name),
        "PATH=/system/commands".into(),
        format!("TERM={}", terminal_type(term)),
        "LANG=C.UTF-8".into(),
        "EDITOR=nano".into(),
    ]
}

/// The block `exec` takes (radix-process(2)): `args` with `env`, in `/`.
pub fn exec_block(path: &[u8], args: &[&[u8]], env: &[String]) -> Vec<u8> {
    let mut block = Vec::new();
    let header = [process::EXEC_VERSION, args.len() as u32, env.len() as u32, 2];
    for word in header {
        block.extend_from_slice(&word.to_le_bytes());
    }
    let strings =
        args.iter().copied().chain(env.iter().map(String::as_bytes)).chain([b"/".as_slice(), path]);
    for string in strings {
        block.extend_from_slice(string);
        block.push(0);
    }
    block
}

/// Slots of our own to put capabilities in, for as long as they live:
/// those of descriptors of /devices/null, emptied once all are open. An
/// empty slot is a free descriptor to the C library, which the next open
/// would take, so emptying each as it opened would give the same slot
/// every time. Closing the descriptors deletes what is in them.
pub struct Spares(Vec<File>);

impl Spares {
    pub fn new(count: usize) -> io::Result<Spares> {
        let files =
            (0..count).map(|_| File::open("/devices/null")).collect::<io::Result<Vec<_>>>()?;
        for file in &files {
            let _ = capability::delete(&SYS, sys::slot_of(file.as_raw_fd()));
        }
        Ok(Spares(files))
    }

    pub fn slot(&self, index: usize) -> u32 {
        sys::slot_of(self.0[index].as_raw_fd())
    }
}

/// Starts `user`'s session on `terminal`, acting for them and the
/// groups `groups`, with the names capability in `names`. Returns the
/// number of its first process, a child of authd's.
pub fn start(
    names: u32,
    user: &User,
    groups: &[u32],
    terminal: &OwnedFd,
    term: &str,
) -> Result<u64, String> {
    let failed = |what: &'static str| {
        move |status: Status| {
            format!("cannot start the session: {what}: {}", sys::status_name(status.0))
        }
    };
    let spares =
        Spares::new(4).map_err(|err| format!("cannot start the session: no slots: {err}"))?;
    let [stamped, root, program, child] = [0, 1, 2, 3].map(|i| spares.slot(i));
    let others: Vec<u8> = groups.iter().flat_map(|g| g.to_le_bytes()).collect();
    names::stamp(&SYS, names, startup::SLOT_ROOT, stamped, user.uid, user.gid, &others)
        .map_err(failed("stamping its root"))?;
    let rights = capability::info(&SYS, stamped).map_err(failed("its root"))?.rights;
    // Without the bypass right, and without control, which would let
    // them mount, unmount and attach on any folder they may walk to, for
    // everyone: that is root's.
    let without = Rights(rights.bits() & !(Rights::BYPASS.bits() | Rights::CONTROL.bits()));
    capability::derive(&SYS, stamped, root, without, 0).map_err(failed("its root"))?;
    // The program is opened through the user's root, so that it is one
    // they may run.
    let path = FIRST_PROGRAM.trim_start_matches('/');
    directory::open(&SYS, root, path.as_bytes(), OpenFlags::NONE, Rights::READ, program, 0)
        .map_err(failed("opening login"))?;
    process::create(&SYS, startup::SLOT_PROCESS, ObjectKind::Process, SLOTS, child)
        .map_err(failed("making its process"))?;
    names::name_process(&SYS, names, child, user.uid, user.gid, &others)
        .map_err(failed("naming its process"))?;
    let tty = sys::slot_of(terminal.as_raw_fd());
    let shares = [
        (root, startup::SLOT_ROOT),
        (startup::SLOT_RANDOM, startup::SLOT_RANDOM),
        (startup::SLOT_LOG, startup::SLOT_LOG),
        (tty, startup::SLOT_FD_BASE),
        (tty, startup::SLOT_FD_BASE + 1),
        (tty, startup::SLOT_FD_BASE + 2),
    ];
    for (from, to) in shares {
        process::share(&SYS, child, from, to, Rights::ALL, 0)
            .map_err(failed("giving it its capabilities"))?;
    }
    let block =
        exec_block(FIRST_PROGRAM.as_bytes(), &[b"login", b"--session"], &environment(user, term));
    process::exec(&SYS, child, program, &block).map_err(failed("starting login"))?;
    // The child stays in the tree after its capability goes, for wait.
    let number = process::info(&SYS, child).map_err(failed("its number"))?.number;
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> User {
        accounts::users("alice:x:1000:1000::/users/alice:/system/commands/nukesh\n")
            .entries()
            .next()
            .cloned()
            .unwrap()
    }

    #[test]
    fn a_session_starts_with_a_login_environment() {
        let env = environment(&alice(), "linux");
        for set in ["HOME=/users/alice", "USER=alice", "LOGNAME=alice", "TERM=linux"] {
            assert!(env.iter().any(|e| e == set), "{set}");
        }
        assert!(environment(&alice(), "x\ny").iter().any(|e| e == "TERM=dumb"));
        assert!(environment(&alice(), "").iter().any(|e| e == "TERM=dumb"));
    }

    #[test]
    fn the_block_holds_the_arguments_then_the_environment() {
        let block = exec_block(b"/p", &[b"p", b"-s"], &["A=1".into()]);
        let words: Vec<u32> =
            block[..16].chunks(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect();
        assert_eq!(words, [process::EXEC_VERSION, 2, 1, 2]);
        assert_eq!(&block[16..], b"p\0-s\0A=1\0/\0/p\0");
    }
}
