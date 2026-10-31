// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AArch64 stubs of the Radix hardware abstraction layer.
//!
//! Dukovo 1.0 runs on x86-64 only, but the kernel is meant to be portable.
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
    Aml, BootParams, Cpu, CpuInfo, CpuLocal, CpuPlace, Devices, Dma, Fault, FaultTest,
    InterruptController, Ipi, MapPageError, Mmio, MsiMessage, PageEdit, PageTable, PciSegment,
    Platform, Scanned, Shootdowns, SignalFrame, Smp, SyscallRegs, Threads, Timer, UserAccess,
    UserFault, UserMode,
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

    fn virtualized() -> bool {
        false
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

    fn machine_check_banks() -> usize {
        0
    }

    fn scalable_machine_checks() -> bool {
        false
    }

    fn poll_machine_checks(_found: &mut radix_hal::MachineCheckFound<'_>) {}

    fn corrected_error_interrupt() -> Option<&'static str> {
        None
    }

    fn set_corrected_error_handler(_handler: fn()) {}

    fn mask_corrected_errors(_masked: bool) {}

    fn performance_driver() -> Option<radix_pstate::Driver> {
        None
    }

    fn start_performance() -> Option<(radix_pstate::Levels, radix_pstate::Request)> {
        None
    }

    fn request_performance(_request: radix_pstate::Request) {}

    fn performance_counts() -> Option<(u64, u64)> {
        None
    }

    fn performance_scale(_levels: radix_pstate::Levels) -> Option<(radix_pstate::Scale, u8)> {
        None
    }

    fn chipset_temperatures(_found: &mut dyn FnMut(radix_sensors::k10temp::Sensor)) {}

    fn read_chipset_temperature(_sensor: radix_sensors::k10temp::Sensor) -> Option<i32> {
        None
    }

    fn core_temperature() -> Option<radix_sensors::coretemp::Sample> {
        None
    }
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

    fn interrupted_user() -> bool {
        false
    }

    fn running_word() -> usize {
        RUNNING.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn set_running_word(word: usize) {
        RUNNING.store(word, core::sync::atomic::Ordering::Relaxed);
    }
}

/// The running-thread word of the stubs' single CPU.
static RUNNING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

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

    fn stopped_trace(_index: usize, _out: &mut [usize]) -> usize {
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

    fn ipis_sent(_cpu: usize) -> u64 {
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

    fn timer_writes(_cpu: usize) -> u64 {
        0
    }

    fn timer_mode() -> &'static str {
        "none"
    }

    fn wall_clock_ns() -> Option<u64> {
        None
    }
}

impl Devices for Arch {
    fn pci_segments(_found: &mut dyn FnMut(PciSegment)) {}

    fn map_device(_phys: u64, _len: usize) -> Option<Mmio> {
        None
    }

    fn alloc_dma(_source: radix_hal::PciAddress, _pages: usize, _below_4g: bool) -> Option<Dma> {
        None
    }

    fn map_dma(_source: radix_hal::PciAddress, _frames: &[(u64, u32)]) -> bool {
        true
    }

    fn unmap_dma(_source: radix_hal::PciAddress, _frames: &[(u64, u32)]) {}

    fn alloc_cache_page() -> Option<radix_hal::Page> {
        None
    }

    fn allocate_interrupt(
        _source: radix_hal::PciAddress,
        _handler: fn(usize),
        _arg: usize,
    ) -> Option<(u32, MsiMessage)> {
        None
    }

    fn allocate_interrupt_at(
        _source: radix_hal::PciAddress,
        _cpu: usize,
        _handler: fn(usize),
        _arg: usize,
    ) -> Option<(u32, MsiMessage)> {
        None
    }

    fn free_interrupt(_vector: u32) {}

    fn interrupt_count(_vector: u32) -> u64 {
        0
    }

    fn route_interrupt(
        _gsi: u32,
        _level: bool,
        _active_low: bool,
        _handler: fn(usize),
        _arg: usize,
    ) -> Option<u32> {
        None
    }

    fn start_keyboard(_byte: fn(u8)) -> Option<&'static str> {
        None
    }

    fn keyboard_send(_byte: u8) -> bool {
        false
    }

    fn start_power_button(_pressed: fn()) -> Option<&'static str> {
        None
    }

    fn find_watchdog() -> Option<radix_hal::Watchdog> {
        None
    }

    fn watchdog_start(seconds: u32) -> u32 {
        seconds
    }

    fn watchdog_pet() {}

    fn watchdog_stop() {}

    fn find_smbus(_wait: fn(u64)) -> bool {
        false
    }

    fn smbus_transfer(
        _address: u8,
        _transfer: radix_hal::SmbusTransfer,
    ) -> Result<u16, radix_hal::SmbusError> {
        Err(radix_hal::SmbusError::NoController)
    }

    fn smbus_name(_out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        Err(core::fmt::Error)
    }

    fn ec_read(_port: u16) -> u8 {
        0xff
    }

    fn ec_write(_port: u16, _value: u8) {}
}

/// A placeholder thread context.
#[derive(Debug)]
pub struct Context;

/// A placeholder address space.
#[derive(Clone, Debug)]
pub struct Space;

/// A placeholder shared area.
#[derive(Debug)]
pub struct Shared;

impl Aml for Arch {
    fn acpi_rsdp() -> Option<u64> {
        None
    }

    fn map_firmware(_phys: u64, _len: usize) -> Option<usize> {
        None
    }

    fn unmap_firmware(_virt: usize, _len: usize) {}

    fn read_port(_port: u16, _width: u8) -> u32 {
        u32::MAX
    }

    fn write_port(_port: u16, _width: u8, _value: u32) {}

    fn start_firmware_interrupt(_irq: u32, _handler: fn()) -> bool {
        false
    }

    fn stop_firmware_interrupt() {}
}

impl UserMode for Arch {
    type Space = Space;
    type Shared = Shared;
    type UserRegisters = ();

    const USER_TOP: u64 = 0x0000_7fff_ffff_f000;

    fn new_space() -> Option<Space> {
        None
    }

    fn map_page(_: &Space, _: u64, _: u64, _: UserAccess) -> Result<(), MapPageError> {
        Err(MapPageError::NoMemory)
    }

    fn map_pages(_: &Space, _: u64, _: u64, _: u64, _: UserAccess) -> Result<(), MapPageError> {
        Err(MapPageError::NoMemory)
    }

    fn split_pages(_: &Space, _: u64) -> Result<(), MapPageError> {
        Ok(())
    }

    fn unmap_page(_: &Space, _: u64) -> Option<u64> {
        None
    }

    fn query_page(_: &Space, _: u64) -> Option<(u64, UserAccess)> {
        None
    }

    fn query_entry(_: &Space, _: u64) -> Option<(u64, Option<UserAccess>)> {
        None
    }

    fn set_entry(
        _: &Space,
        _: u64,
        _: u64,
        _: Option<UserAccess>,
        _: bool,
    ) -> Result<(), MapPageError> {
        Err(MapPageError::NoMemory)
    }

    fn frames(_: &Space, _: u64, _: u64, _: bool, _: &mut dyn FnMut(u64) -> bool) {}

    fn scan(_: &Space, _: u64, _: u64, _: bool, _: &mut dyn FnMut(u64, Scanned) -> bool) {}

    fn swap_out(_: &Space, _: u64, _: u64, _: u64) -> bool {
        false
    }

    fn retire_page(_: &Space, _: u64, _: u64, _: u64) -> bool {
        false
    }

    fn set_swapped(_: &Space, _: u64, _: u64) -> Result<(), MapPageError> {
        Err(MapPageError::NoMemory)
    }

    fn swapped(_: &Space, _: u64) -> Option<u64> {
        None
    }

    fn clear_swapped(_: &Space, _: u64, _: u64, _: &mut dyn FnMut(u64)) {}

    fn edit_pages(
        _: &Space,
        _: u64,
        _: u64,
        _: &mut dyn FnMut(u64, u64, Option<UserAccess>) -> PageEdit,
        _: &mut dyn FnMut(u64),
    ) {
    }

    fn copy_pages(
        _: &Space,
        _: &Space,
        _: u64,
        _: u64,
        _: &mut dyn FnMut(u64, u64, Option<UserAccess>) -> (PageEdit, Option<Option<UserAccess>>),
    ) -> Result<(), u64> {
        Ok(())
    }

    fn free_page_tables(_: &Space) {}

    fn attach_space(_: &Context, _: &Space) -> bool {
        false
    }

    fn set_thread_pointer(_: &Context, _: u64) {}

    fn enter_user(_: u64, _: u64, _: u64) -> ! {
        Self::halt()
    }

    fn set_syscall_handler(_: fn(&mut SyscallRegs)) {}

    fn set_user_fault_handler(_: fn(&UserFault)) {}

    fn set_user_page_handler(_: fn(u64, bool) -> bool) {}

    fn set_user_return_handler(_: fn()) {}

    fn user_registers() -> Option<()> {
        None
    }

    fn set_results((): &mut (), _: &SyscallRegs) {}

    fn thread_pointer((): &()) -> u64 {
        0
    }

    fn enter_user_registers((): &()) -> ! {
        Self::halt()
    }

    fn copy_extended_state(_: &Context) {}

    fn user_stack_pointer() -> u64 {
        0
    }

    fn push_signal_frame(_: &SignalFrame<'_>) -> bool {
        false
    }

    fn pop_signal_frame(_: u64, _: &mut SyscallRegs) -> Option<u64> {
        None
    }

    fn restart_syscall(_: &SyscallRegs) {}

    fn copy_from_user(_: &mut [u8], _: u64) -> bool {
        false
    }

    fn copy_to_user(_: u64, _: &[u8]) -> bool {
        false
    }

    fn new_shared(_: usize) -> Option<Shared> {
        None
    }

    fn shared_phys(_: &Shared, _: usize) -> u64 {
        0
    }

    fn shared_load(_: &Shared, _: usize) -> u32 {
        0
    }

    fn shared_store(_: &Shared, _: usize, _: u32) {}

    fn shared_read(_: &Shared, _: usize, _: &mut [u8]) {}

    fn shared_write(_: &Shared, _: usize, _: &[u8]) {}

    fn zero_frames(_: u64, _: u64) {}

    fn write_frames(_: u64, _: &[u8]) {}

    fn copy_frame(_: u64, _: u64) {}

    fn read_frames(_: u64, _: &mut [u8]) {}

    fn cycle_counter_hz() -> u64 {
        0
    }

    fn describe_user_mode(_: &mut dyn FnMut(core::fmt::Arguments<'_>)) {}
}

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

    fn shootdowns(_cpu: usize) -> Shootdowns {
        Shootdowns::default()
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

    fn power_off() {}

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

    fn smbios() -> Option<radix_hal::Smbios> {
        None
    }

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

    fn self_test_msr() -> Option<Result<Option<u64>, &'static str>> {
        None
    }
}
