// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's own interface (radix-ipc(7)), for what the C library does
//! not wrap: CPU affinity, I/O priority and tracing (radix-process(2)),
//! and clearing the kernel log (radix-kernel-log(2)). The calls go
//! through `radix_abi`'s wrappers, generated from `abi/*.ridl`.
//!
//! Built for another system (the unit tests run on the build machine),
//! every call fails with no_such_syscall.

use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use radix_abi::capability::{self, Rights};
use radix_abi::directory::{self, OpenFlags};
use radix_abi::errors::{File, Kernel};
use radix_abi::{Status, Syscalls, startup};

/// The kernel, through the `syscall` instruction.
pub struct Sys;

impl Syscalls for Sys {
    #[cfg(target_os = "radix")]
    #[inline(always)]
    fn syscall(&self, number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        let mut r = args;
        let status: u64;
        // SAFETY: the kernel reads only these registers, writes only these
        // and rcx and r11 (declared clobbered), and touches no memory of
        // the program's but the buffers the arguments name, which the
        // wrappers pass from references they hold; it does not use the
        // program's stack.
        unsafe {
            std::arch::asm!(
                "syscall",
                inlateout("rax") number => status,
                inlateout("rdi") r[0],
                inlateout("rsi") r[1],
                inlateout("rdx") r[2],
                inlateout("r10") r[3],
                inlateout("r8") r[4],
                inlateout("r9") r[5],
                inlateout("r12") r[6],
                inlateout("r13") r[7],
                inlateout("r14") r[8],
                inlateout("r15") r[9],
                out("rcx") _,
                out("r11") _,
                options(nostack),
            );
        }
        (status, r)
    }

    #[cfg(not(target_os = "radix"))]
    fn syscall(&self, _number: u64, args: [u64; 10]) -> (u64, [u64; 10]) {
        (Kernel::NoSuchSyscall.status(0).0, args)
    }
}

/// The kernel, for `radix_abi`'s wrappers.
pub static SYS: Sys = Sys;

/// The slot of the program's own process.
pub const SLOT_PROCESS: u32 = startup::SLOT_PROCESS;

/// The slot of file descriptor `fd` (radix-posix(7)).
pub fn slot_of(fd: i32) -> u32 {
    startup::SLOT_FD_BASE + fd as u32
}

/// The name of slot `slot` of a program's capability space: `fd N` for a
/// descriptor's, the startup slots by what they hold (radix-startup(2)).
pub fn slot_name(slot: u32) -> String {
    if slot >= startup::SLOT_FD_BASE {
        return format!("fd {}", slot - startup::SLOT_FD_BASE);
    }
    let name = match slot {
        startup::SLOT_THREAD => "thread",
        startup::SLOT_SPACE => "space",
        startup::SLOT_RANDOM => "random",
        startup::SLOT_LOG => "log",
        startup::SLOT_PROCESS => "process",
        startup::SLOT_ROOT => "root",
        _ => return format!("slot {slot}"),
    };
    name.to_string()
}

/// A status's error by name, `no_such_file` or `file.not_found`, and
/// its detail if it has one.
pub fn status_name(status: u64) -> String {
    let status = Status(status);
    let name = if let Some(error) = Kernel::from_status(status) {
        error.name().to_string()
    } else if let Some(error) = File::from_status(status) {
        format!("file.{}", error.name())
    } else {
        format!("error {:#x}", status.0)
    };
    match status.detail() {
        0 => name,
        detail => format!("{name} ({detail})"),
    }
}

/// The errno a status maps to, as the C library maps it.
pub fn errno_of(status: Status) -> i32 {
    if let Some(error) = Kernel::from_status(status) {
        error.errno()
    } else if let Some(error) = File::from_status(status) {
        error.errno()
    } else {
        libc::EIO
    }
}

/// A status as an I/O error.
pub fn io_error(status: Status) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno_of(status))
}

/// Opens the directory `path` with the control right, which the C
/// library's `open` never asks for and trimming a filesystem or replacing
/// one of its disks needs (radix-directory(2)): it opens `path` again from
/// the root, into the slot of a descriptor the C library opened, which
/// then closes it. Fails with permission denied when the root has no such
/// right to hand on.
pub fn open_control(path: &Path) -> std::io::Result<std::fs::File> {
    let absolute = std::fs::canonicalize(path)?;
    let dir = std::fs::File::open(&absolute)?;
    let slot = slot_of(dir.as_raw_fd());
    let text = match absolute.as_os_str().as_bytes().strip_prefix(b"/") {
        Some([]) | None => b".".as_slice(),
        Some(rest) => rest,
    };
    let _ = capability::delete(&SYS, slot);
    let rights = Rights::READ.union(Rights::CONTROL);
    directory::open(&SYS, startup::SLOT_ROOT, text, OpenFlags::DIRECTORY, rights, slot)
        .map_err(io_error)?;
    Ok(dir)
}
