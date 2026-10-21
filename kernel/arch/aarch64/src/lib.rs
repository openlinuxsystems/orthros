// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AArch64 stubs of the Radix hardware abstraction layer.
//!
//! OrthrOS 1.0 runs on x86-64 only, but the kernel is meant to be portable.
//! This crate implements every HAL trait with placeholders so that CI can
//! check the architecture-independent kernel for `aarch64-unknown-none`
//! (`ob lint` runs `cargo clippy` for that target). Code in `kernel/radix`
//! that reaches past the HAL into x86-64 details fails that check.
//!
//! Nothing here runs: there is no AArch64 boot path, so the stubs only
//! have to compile. A real port replaces them.

#![no_std]
#![forbid(unsafe_code)]

use core::ops::Range;
use core::sync::atomic::AtomicU32;

use radix_hal::{
    BootParams, Cpu, CpuInfo, CpuLocal, CpuPlace, Fault, FaultTest, InterruptController, Ipi,
    PageTable, Platform, Smp, Threads, Timer,
};

/// The AArch64 implementation of the HAL traits.
#[derive(Clone, Copy, Debug)]
pub struct Arch;

impl Cpu for Arch {
    const NAME: &'static str = "aarch64";

    fn halt() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }

    fn wait_for_interrupt() {
        core::hint::spin_loop();
    }

    fn idle_until(_wake: &AtomicU32) {
        core::hint::spin_loop();
    }

    fn idle_mode() -> (bool, &'static str) {
        (false, "none")
    }

    fn interrupts_enabled() -> bool {
        false
    }

    fn enable_interrupts() {}

    fn disable_interrupts() -> bool {
        false
    }

    fn frame_pointer() -> usize {
        0
    }

    fn backtrace(_frame_pointer: usize, _out: &mut [usize]) -> usize {
        0
    }

    fn breakpoint() {}

    fn flush_to_memory(_bytes: &[u8]) {}
}

impl CpuLocal for Arch {
    fn current_cpu() -> usize {
        0
    }

    fn preempt_disable() {}

    fn preempt_enable() -> u32 {
        0
    }

    fn preempt_count() -> u32 {
        0
    }

    fn set_preempt_count(_count: u32) {}

    fn interrupt_depth() -> u32 {
        0
    }
}

impl Smp for Arch {
    fn cpu_count() -> usize {
        1
    }

    fn cpu_info(index: usize) -> Option<CpuInfo> {
        (index == 0).then_some(CpuInfo { index: 0, hardware_id: 0, boot_cpu: true, online: true })
    }

    fn start_secondary_cpus(_entry: fn(usize) -> !) -> usize {
        1
    }

    fn stop_other_cpus() -> usize {
        0
    }

    fn cpu_place(index: usize) -> Option<CpuPlace> {
        (index == 0).then_some(CpuPlace::default())
    }
}

impl InterruptController for Arch {
    fn name() -> &'static str {
        "none"
    }

    fn send_ipi(_cpu: usize, _ipi: Ipi) {}

    fn pings_received(_cpu: usize) -> u64 {
        0
    }
}

impl Timer for Arch {
    fn now_ns() -> u64 {
        0
    }

    fn tick_hz() -> u32 {
        0
    }

    fn ticks(_cpu: usize) -> u64 {
        0
    }

    fn delay_us(_us: u64) {}

    fn set_timer_handler(_handler: fn(bool)) {}

    fn set_deadline(_deadline_ns: u64) {}

    fn timer_mode() -> &'static str {
        "none"
    }
}

/// A placeholder thread context.
#[derive(Debug)]
pub struct Context;

impl Threads for Arch {
    type Context = Context;

    fn adopt_current() -> Option<Context> {
        None
    }

    fn new_context(_entry: fn(usize) -> !, _arg: usize, _stack_pages: usize) -> Option<Context> {
        None
    }

    fn switch(_from: &Context, _to: &Context) {}

    fn is_running(_context: &Context) -> bool {
        false
    }

    fn stack(_context: &Context) -> Range<usize> {
        0..0
    }

    fn set_preempt_handler(_handler: fn()) {}

    fn extended_state() -> &'static str {
        "none"
    }

    fn self_test_fill_vectors(_seed: u64) {}

    fn self_test_vectors_hold(_seed: u64) -> bool {
        true
    }
}

impl PageTable for Arch {
    fn translate(_virt: usize) -> Option<u64> {
        None
    }

    fn read(_virt: usize, _buf: &mut [u8]) -> bool {
        false
    }

    fn kernel_slide() -> u64 {
        0
    }

    fn describe(_line: &mut dyn FnMut(core::fmt::Arguments<'_>)) {}

    fn map_area(_pages: usize) -> Option<usize> {
        None
    }

    fn unmap_area(_virt: usize, _pages: usize) {}

    fn self_test_tlb(_on_others: &dyn Fn(fn())) -> Result<(), &'static str> {
        Err("not available on aarch64")
    }
}

impl Platform for Arch {
    fn init(_params: &BootParams) {}

    fn current_fault() -> Option<Fault> {
        None
    }

    fn set_breakpoint_handler(_handler: fn(&Fault)) {}

    fn reset() -> ! {
        Self::halt()
    }

    fn console_read() -> Option<u8> {
        None
    }

    fn key_pressed() -> bool {
        false
    }

    fn take_crash_log_region() -> Option<&'static mut [u8]> {
        None
    }

    fn emulator_exit(_success: bool) {}

    fn describe_firmware(_line: &mut dyn FnMut(core::fmt::Arguments<'_>)) {}

    fn has_debug_console() -> bool {
        false
    }

    fn console_write(_text: &str) {}

    fn kernel_symbols() -> &'static [u8] {
        &[]
    }

    fn self_test_fault(_test: FaultTest) -> &'static str {
        "not available on aarch64"
    }
}
