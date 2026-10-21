// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Faults, one per program: the kernel must end the program that makes
//! it, and nothing else. Each is an instruction in assembly, so the
//! compiler cannot reason it away.

use libradix::abi::startup::STARTUP_ADDRESS;
use libradix::println;

/// Makes fault `which` (the kernel's `FAULTS` list). Returns only if the
/// fault did not happen, which is a failure.
pub fn run(which: u64) -> u64 {
    println!("making fault {which}");
    // SAFETY: none of these is safe; each is meant to fault, and the
    // kernel ends the program before it returns. Should one not fault,
    // the program reports it and exits.
    unsafe {
        match which {
            0 => core::arch::asm!("mov rax, [0]", out("rax") _),
            1 => core::arch::asm!("mov qword ptr [{0}], 1", in(reg) STARTUP_ADDRESS),
            2 => {
                // `ret` on the stack, which is never executable.
                let code = [0xc3u8; 16];
                core::arch::asm!("call {0}", in(reg) code.as_ptr(), clobber_abi("C"));
            }
            3 => core::arch::asm!("hlt"),
            4 => core::arch::asm!("jmp {0}", in(reg) 0xffff_ffff_8000_0000u64),
            5 => core::arch::asm!("ud2"),
            6 => core::arch::asm!("int3"),
            7 => core::arch::asm!(
                "xor edx, edx",
                "div {0}",
                in(reg) 0u64,
                inout("rax") 1u64 => _,
                out("rdx") _,
            ),
            _ => {
                println!("no fault {which}");
                return 1;
            }
        }
    }
    println!("fault {which} did not happen");
    1
}
