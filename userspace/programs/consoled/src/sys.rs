// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel's own requests on the devices consoled drives, which the C
//! library has no names for: radix-framebuffer(2)'s and radix-input(2)'s
//! `control` requests (radix-file(2)), through `radix_abi`'s wrappers.
//!
//! Built for another system (the unit tests run on the build machine),
//! every call fails with no_such_syscall.

use std::io;

use radix_abi::errors::{File, Kernel};
use radix_abi::{Status, Syscalls, file, startup};

/// The kernel, through the `syscall` instruction.
struct Sys;

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

static SYS: Sys = Sys;

/// A status as an I/O error, with the errno the C library maps it to.
fn io_error(status: Status) -> io::Error {
    let errno = if let Some(error) = Kernel::from_status(status) {
        error.errno()
    } else if let Some(error) = File::from_status(status) {
        error.errno()
    } else {
        libc::EIO
    };
    io::Error::from_raw_os_error(errno)
}

/// Sends the device open as descriptor `fd` its request `request` with
/// `argument`, and `buffer` for it to read or fill (radix-file(2)'s
/// `control`).
pub fn control(fd: i32, request: u32, argument: u64, buffer: &mut [u8]) -> io::Result<u64> {
    let slot = startup::SLOT_FD_BASE + fd as u32;
    let address = if buffer.is_empty() { 0 } else { buffer.as_mut_ptr() as u64 };
    file::control(&SYS, slot, request, argument, address, buffer.len() as u64)
        .map(|done| done.result)
        .map_err(io_error)
}
