// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The RISC-V smoke boot (ADR 0270): `radix_arch_riscv64::smoke` is the
//! whole of it; this is the program OpenSBI starts. Built for any other
//! target, it is empty.

#![cfg_attr(target_arch = "riscv64", no_std, no_main)]

#[cfg(target_arch = "riscv64")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    radix_arch_riscv64::smoke::panic(info)
}

#[cfg(not(target_arch = "riscv64"))]
fn main() {}
