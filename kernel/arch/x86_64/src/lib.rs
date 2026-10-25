// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! x86-64 support for Radix: the implementation of the hardware abstraction
//! layer on PC hardware.
//!
//! This crate owns the machine from GRUB's jump to `_start` until it calls
//! the kernel's `radix_kernel_main`: the boot trampoline ([`boot`]), which
//! seeds the random number generator ([`random`]) and moves the kernel
//! ([`kaslr`]) before jumping to [`entry`], the CPU's descriptor tables
//! ([`percpu`], [`interrupts`]), physical memory and the kernel's page
//! tables ([`memory`], [`paging`]), the consoles it knows about
//! ([`serial`], [`vga`]) and early microcode ([`microcode`]). Later,
//! through [`Platform::init`], it reads the ACPI tables ([`acpi`]), sets up
//! the interrupt controllers ([`apic`]) and timers ([`time`]), and it starts
//! and stops the other CPUs ([`smp`]). Kernel threads' contexts and the
//! context switch are in [`thread`], their FPU and vector state in [`fpu`].
//! Programs' address spaces are in [`space`], and the SYSCALL entry,
//! entering user mode and user copies in [`user`] (Phase 5). [`power`]
//! restarts the machine, and [`cpu`] and [`io`] wrap single instructions.
//! [`devices`] maps device registers, allocates DMA memory and hands out
//! interrupt vectors for message-signalled interrupts, and [`rtc`] reads
//! the real-time clock (Phase 7).
//!
//! It is part of the trusted computing base, so it may use `unsafe`, and
//! every `unsafe` block states why it is sound. docs/design/boot.md and
//! docs/design/interrupts.md describe the design.

#![no_std]

mod acpi;
mod apic;
mod boot;
mod cpu;
mod devices;
mod entry;
mod fpu;
mod i8042;
mod interrupts;
mod io;
mod kaslr;
mod kvm;
mod memory;
mod microcode;
mod paging;
mod percpu;
mod power;
mod random;
mod rtc;
mod serial;
mod smp;
mod space;
mod thread;
mod time;
mod user;
mod vga;

use core::fmt;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use radix_hal::cmdline::CommandLine;
use radix_hal::{
    BootParams, Cpu, CpuInfo, CpuLocal, CpuPlace, Devices, Fault, FaultTest, InterruptController,
    Ipi, MapPageError, PageEdit, PageTable, PciSegment, Platform, SignalFrame, Smp, SyscallRegs,
    Threads, Timer, UserAccess, UserFault, UserMode,
};

/// Whether idle CPUs wait with MONITOR and MWAIT.
static IDLE_MWAIT: AtomicBool = AtomicBool::new(false);

/// The x86-64 implementation of the HAL traits.
#[derive(Clone, Copy, Debug)]
pub struct Arch;

impl Cpu for Arch {
    const NAME: &'static str = "x86_64";

    fn halt() -> ! {
        cpu::halt_forever()
    }

    fn wait_for_interrupt() {
        cpu::halt();
    }

    fn idle_until(wake: &AtomicU32) {
        if IDLE_MWAIT.load(Ordering::Relaxed) {
            cpu::monitor(core::ptr::from_ref::<AtomicU32>(wake) as usize);
            // A write between the check and MWAIT still ends the wait: the
            // monitor is armed already.
            if wake.load(Ordering::Acquire) == 0 {
                cpu::sti_mwait();
            }
        } else if wake.load(Ordering::Acquire) == 0 {
            cpu::sti_hlt();
        }
        cpu::disable_interrupts();
    }

    fn idle_mode() -> (bool, &'static str) {
        if IDLE_MWAIT.load(Ordering::Relaxed) { (true, "MWAIT") } else { (false, "HLT") }
    }

    fn virtualized() -> bool {
        cpu::feature::hypervisor()
    }

    fn interrupts_enabled() -> bool {
        cpu::interrupts_enabled()
    }

    fn enable_interrupts() {
        cpu::enable_interrupts();
    }

    fn disable_interrupts() -> bool {
        cpu::disable_interrupts()
    }

    fn frame_pointer() -> usize {
        cpu::frame_pointer()
    }

    fn backtrace(frame_pointer: usize, out: &mut [usize]) -> usize {
        let cpu = percpu::current();
        let mut fp = frame_pointer;
        let mut count = 0;
        while count < out.len() {
            let Some(stack) = cpu.stack_containing(fp) else { break };
            if !fp.is_multiple_of(8) || fp + 16 > stack.end {
                break;
            }
            // SAFETY: `fp` and `fp + 8` lie inside one of this CPU's kernel
            // stacks, which are mapped for the kernel's whole life, or the
            // running thread's, which stays mapped while it runs (checked
            // above); volatile reads of them have no side effects.
            let (next, return_address) = unsafe {
                (
                    core::ptr::read_volatile(fp as *const usize),
                    core::ptr::read_volatile((fp + 8) as *const usize),
                )
            };
            // The outermost frame returns into the boot trampoline, which
            // is not kernel code; a corrupt record points anywhere.
            if !paging::is_kernel_text(return_address) {
                break;
            }
            out[count] = return_address;
            count += 1;
            // Frames move up the stack; a frame record pointing down or
            // sideways on the same stack ends the walk.
            if next <= fp && cpu.stack_containing(next) == Some(stack.clone()) {
                break;
            }
            fp = next;
        }
        count
    }

    fn breakpoint() {
        cpu::breakpoint();
    }

    fn flush_to_memory(bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if cpu::feature::clflush() {
            let start = bytes.as_ptr() as usize & !63;
            let end = bytes.as_ptr() as usize + bytes.len();
            for line in (start..end).step_by(64) {
                cpu::clflush(line);
            }
            cpu::mfence();
        } else {
            cpu::wbinvd();
        }
    }
}

impl CpuLocal for Arch {
    fn current_cpu() -> usize {
        percpu::current_index()
    }

    fn preempt_disable() {
        percpu::preempt_disable();
    }

    fn preempt_enable() -> u32 {
        percpu::preempt_enable()
    }

    fn preempt_count() -> u32 {
        percpu::preempt_count()
    }

    fn set_preempt_count(count: u32) {
        percpu::set_preempt_count(count);
    }

    fn interrupt_depth() -> u32 {
        percpu::interrupt_depth()
    }

    fn interrupted_user() -> bool {
        percpu::interrupted_user()
    }

    fn running_word() -> usize {
        percpu::running_word()
    }

    fn set_running_word(word: usize) {
        percpu::set_running_word(word);
    }
}

impl Smp for Arch {
    fn cpu_count() -> usize {
        smp::count()
    }

    fn cpu_info(index: usize) -> Option<CpuInfo> {
        smp::info(index)
    }

    fn start_secondary_cpus(entry: fn(usize) -> !) -> usize {
        let online = smp::start_all(entry);
        let (used, span) = memory::allocated();
        radix_klog::info!(
            "Memory: {} KiB allocated at boot for page tables and stacks, in {:#x}-{:#x}",
            used / 1024,
            span.start,
            span.end.saturating_sub(1)
        );
        let updated = microcode::ap_updates();
        if updated > 0 {
            radix_klog::info!("microcode: {updated} other CPUs loaded the update too");
        }
        online
    }

    fn stop_other_cpus() -> usize {
        smp::stop_others()
    }

    fn stopped_trace(index: usize, out: &mut [usize]) -> usize {
        let Some(cpu) = percpu::get(index).filter(|cpu| cpu.stopped.load(Ordering::Acquire)) else {
            return 0;
        };
        let frames = cpu.stopped_at.iter().map(|slot| slot.load(Ordering::Relaxed));
        let mut len = 0;
        for (out, address) in out.iter_mut().zip(frames.take_while(|&address| address != 0)) {
            *out = address;
            len += 1;
        }
        len
    }

    fn cpu_place(index: usize) -> Option<CpuPlace> {
        smp::place(index)
    }
}

impl InterruptController for Arch {
    fn name() -> &'static str {
        apic::mode_name()
    }

    fn send_ipi(cpu: usize, ipi: Ipi) {
        let Some(target) = percpu::get(cpu) else { return };
        let vector = match ipi {
            Ipi::Ping => interrupts::VECTOR_PING,
            Ipi::Reschedule => interrupts::VECTOR_RESCHEDULE,
        };
        apic::send_fixed(target.apic_id.load(Ordering::Relaxed), vector);
    }

    fn pings_received(cpu: usize) -> u64 {
        percpu::get(cpu).map_or(0, |c| c.pings.load(Ordering::Relaxed))
    }

    fn ipis_sent(cpu: usize) -> u64 {
        percpu::get(cpu).map_or(0, |c| c.ipis_sent.load(Ordering::Relaxed))
    }
}

impl Timer for Arch {
    fn now_ns() -> u64 {
        time::now_ns()
    }

    fn tick_hz() -> u32 {
        time::tick_hz()
    }

    fn ticks(cpu: usize) -> u64 {
        percpu::get(cpu).map_or(0, |c| c.ticks.load(Ordering::Relaxed))
    }

    fn delay_us(us: u64) {
        time::delay_us(us);
    }

    fn set_timer_handler(handler: fn(bool)) {
        time::set_handler(handler);
    }

    fn set_deadline(deadline_ns: u64) {
        time::set_deadline(deadline_ns);
    }

    fn timer_writes(cpu: usize) -> u64 {
        time::writes(cpu)
    }

    fn timer_mode() -> &'static str {
        time::mode_name()
    }

    fn wall_clock_ns() -> Option<u64> {
        rtc::wall_clock_ns()
    }
}

impl Devices for Arch {
    fn pci_segments(found: &mut dyn FnMut(PciSegment)) {
        let Some(mcfg) = acpi::get().and_then(|acpi| acpi.mcfg.as_ref()) else { return };
        for window in radix_acpi::mcfg_entries(mcfg) {
            found(PciSegment {
                base: window.base,
                segment: window.segment,
                first_bus: window.start_bus,
                last_bus: window.end_bus,
            });
        }
    }

    fn map_device(phys: u64, len: usize) -> Option<radix_hal::Mmio> {
        devices::map(phys, len)
    }

    fn alloc_dma(pages: usize, below_4g: bool) -> Option<radix_hal::Dma> {
        devices::alloc_dma(pages, below_4g)
    }

    fn alloc_cache_page() -> Option<radix_hal::Page> {
        devices::alloc_cache_page()
    }

    fn allocate_interrupt(handler: fn(usize), arg: usize) -> Option<(u32, radix_hal::MsiMessage)> {
        devices::allocate(handler, arg)
    }

    fn free_interrupt(vector: u32) {
        devices::free(vector);
    }

    fn interrupt_count(vector: u32) -> u64 {
        devices::count(vector)
    }

    fn start_keyboard(byte: fn(u8)) -> Option<&'static str> {
        i8042::start(byte)
    }

    fn keyboard_send(byte: u8) -> bool {
        i8042::send(byte)
    }
}

impl Threads for Arch {
    type Context = thread::Context;

    fn adopt_current() -> Option<Self::Context> {
        thread::Context::adopt_current()
    }

    fn new_context(entry: fn(usize) -> !, arg: usize, stack_pages: usize) -> Option<Self::Context> {
        thread::Context::new(entry, arg, stack_pages)
    }

    fn switch(from: &Self::Context, to: &Self::Context) {
        thread::Context::switch(from, to);
    }

    fn is_running(context: &Self::Context) -> bool {
        context.is_running()
    }

    fn stack(context: &Self::Context) -> Range<usize> {
        context.stack()
    }

    fn set_preempt_handler(handler: fn()) {
        interrupts::set_preempt_handler(handler);
    }

    fn extended_state() -> &'static str {
        fpu::describe()
    }

    fn self_test_fill_vectors(seed: u64) {
        fpu::fill(seed);
    }

    fn self_test_vectors_hold(seed: u64) -> bool {
        fpu::holds(seed)
    }
}

impl PageTable for Arch {
    fn translate(virt: usize) -> Option<u64> {
        paging::translate(virt)
    }

    fn read(virt: usize, buf: &mut [u8]) -> bool {
        paging::read(virt, buf)
    }

    fn kernel_slide() -> u64 {
        kaslr::slide()
    }

    fn describe(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
        let (places, moved) = kaslr::statistics();
        let (map_places, window_places) = memory::layout_places();
        line(format_args!("Random: {}", random::Summary));
        match kaslr::status() {
            kaslr::Status::On => line(format_args!(
                "KASLR: kernel moved by {:#x} (one of {places} places, {moved} addresses \
                 patched); direct map one of {map_places} places, mapping window one of \
                 {window_places}",
                kaslr::slide()
            )),
            kaslr::Status::Disabled => line(format_args!("KASLR: off (nokaslr)")),
            kaslr::Status::NoTable => line(format_args!(
                "KASLR: kernel not moved, the image has no relocation table; direct map one of \
                 {map_places} places, mapping window one of {window_places}"
            )),
            kaslr::Status::NoRoom => line(format_args!(
                "KASLR: kernel not moved, the image is too large; direct map one of \
                 {map_places} places, mapping window one of {window_places}"
            )),
        }
        line(format_args!(
            "Layout: kernel at {:#x}, direct map at {:#x}, mapping window at {:#x}",
            memory::kernel_base() + (1 << 20),
            memory::direct_map(),
            memory::window()
        ));
        paging::describe(line);
    }

    fn map_area(pages: usize) -> Option<usize> {
        paging::map_area(pages).ok()
    }

    fn unmap_area(virt: usize, pages: usize) {
        paging::unmap_area(virt, pages);
    }

    fn self_test_tlb(on_others: &dyn Fn(fn())) -> Result<(), &'static str> {
        space::self_test(on_others)
    }
}

impl UserMode for Arch {
    type Space = space::UserSpace;
    type Shared = user::Shared;
    type UserRegisters = user::UserRegisters;

    const USER_TOP: u64 = user::USER_TOP;

    fn new_space() -> Option<Self::Space> {
        space::UserSpace::new()
    }

    fn map_page(
        space: &Self::Space,
        virt: u64,
        phys: u64,
        access: UserAccess,
    ) -> Result<(), MapPageError> {
        assert!(
            (user::USER_START..user::USER_TOP).contains(&virt),
            "{virt:#x} is outside the user half"
        );
        space.space().map_new(virt, phys, space::access(access))
    }

    fn unmap_page(space: &Self::Space, virt: u64) -> Option<u64> {
        if virt >= user::USER_TOP {
            return None;
        }
        space.space().unmap(virt)
    }

    fn query_page(space: &Self::Space, virt: u64) -> Option<(u64, UserAccess)> {
        if virt >= user::USER_TOP {
            return None;
        }
        space.space().query(virt).map(|(phys, access)| (phys, space::user_access(access)))
    }

    fn query_entry(space: &Self::Space, virt: u64) -> Option<(u64, Option<UserAccess>)> {
        if virt >= user::USER_TOP {
            return None;
        }
        space.space().entry(virt).map(|(phys, access)| (phys, access.map(space::user_access)))
    }

    fn set_entry(
        space: &Self::Space,
        virt: u64,
        phys: u64,
        access: Option<UserAccess>,
        upgrade: bool,
    ) -> Result<(), MapPageError> {
        assert!(
            (user::USER_START..user::USER_TOP).contains(&virt),
            "{virt:#x} is outside the user half"
        );
        space.space().set(virt, phys, access.map(space::access), upgrade)
    }

    fn edit_pages(
        space: &Self::Space,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(u64, u64, Option<UserAccess>) -> PageEdit,
        released: &mut dyn FnMut(u64),
    ) {
        let end = end.min(user::USER_TOP);
        if start >= end {
            return;
        }
        let mut decide = |virt, phys, access: Option<paging::Access>| match decide(
            virt,
            phys,
            access.map(space::user_access),
        ) {
            PageEdit::Keep => space::Edit::Keep,
            PageEdit::Set(access) => space::Edit::Set(access.map(space::access)),
            PageEdit::Clear => space::Edit::Clear,
        };
        space.space().edit(start, end, &mut decide, released);
    }

    fn copy_pages(
        from: &Self::Space,
        to: &Self::Space,
        start: u64,
        end: u64,
        decide: &mut dyn FnMut(
            u64,
            u64,
            Option<UserAccess>,
        ) -> (PageEdit, Option<Option<UserAccess>>),
    ) -> Result<(), u64> {
        let end = end.min(user::USER_TOP);
        if start >= end {
            return Ok(());
        }
        let mut decide = |virt, phys, access: Option<paging::Access>| {
            let (edit, copy) = decide(virt, phys, access.map(space::user_access));
            let edit = match edit {
                PageEdit::Keep => space::Edit::Keep,
                PageEdit::Set(access) => space::Edit::Set(access.map(space::access)),
                PageEdit::Clear => space::Edit::Clear,
            };
            (edit, copy.map(|access| access.map(space::access)))
        };
        from.space().copy_into(to.space(), start, end, &mut decide)
    }

    fn attach_space(context: &Self::Context, space: &Self::Space) -> bool {
        context.attach_space(space)
    }

    fn set_thread_pointer(context: &Self::Context, base: u64) {
        context.set_thread_pointer(base);
    }

    fn enter_user(entry: u64, stack_top: u64, arg: u64) -> ! {
        user::enter_user(entry, stack_top, arg)
    }

    fn set_syscall_handler(handler: fn(&mut SyscallRegs)) {
        user::set_syscall_handler(handler);
    }

    fn set_user_fault_handler(handler: fn(&UserFault)) {
        user::set_fault_handler(handler);
    }

    fn set_user_page_handler(handler: fn(u64, bool) -> bool) {
        user::set_page_handler(handler);
    }

    fn set_user_return_handler(handler: fn()) {
        user::set_return_handler(handler);
    }

    fn user_registers() -> Option<Self::UserRegisters> {
        user::user_registers()
    }

    fn set_results(regs: &mut Self::UserRegisters, results: &SyscallRegs) {
        regs.set_results(results);
    }

    fn thread_pointer(regs: &Self::UserRegisters) -> u64 {
        regs.fs_base
    }

    fn enter_user_registers(regs: &Self::UserRegisters) -> ! {
        user::enter_user_registers(regs)
    }

    fn copy_extended_state(context: &Self::Context) {
        context.copy_extended_state();
    }

    fn user_stack_pointer() -> u64 {
        user::user_stack_pointer()
    }

    fn push_signal_frame(frame: &SignalFrame<'_>) -> bool {
        user::push_signal_frame(frame)
    }

    fn pop_signal_frame(context: u64, regs: &mut SyscallRegs) -> Option<u64> {
        user::pop_signal_frame(context, regs)
    }

    fn restart_syscall(original: &SyscallRegs) {
        user::restart_syscall(original);
    }

    fn copy_from_user(dst: &mut [u8], src: u64) -> bool {
        user::copy_from_user(dst, src)
    }

    fn copy_to_user(dst: u64, src: &[u8]) -> bool {
        user::copy_to_user(dst, src)
    }

    fn new_shared(pages: usize) -> Option<Self::Shared> {
        user::Shared::new(pages)
    }

    fn shared_phys(shared: &Self::Shared, page: usize) -> u64 {
        shared.phys(page)
    }

    fn shared_load(shared: &Self::Shared, offset: usize) -> u32 {
        shared.load(offset)
    }

    fn shared_store(shared: &Self::Shared, offset: usize, value: u32) {
        shared.store(offset, value);
    }

    fn shared_read(shared: &Self::Shared, offset: usize, dst: &mut [u8]) {
        shared.read(offset, dst);
    }

    fn shared_write(shared: &Self::Shared, offset: usize, src: &[u8]) {
        shared.write(offset, src);
    }

    fn zero_frames(phys: u64, pages: u64) {
        user::zero_frames(phys, pages);
    }

    fn write_frames(phys: u64, src: &[u8]) {
        user::write_frames(phys, src);
    }

    fn copy_frame(dst: u64, src: u64) {
        user::copy_frame(dst, src);
    }

    fn read_frames(phys: u64, dst: &mut [u8]) {
        user::read_frames(phys, dst);
    }

    fn cycle_counter_hz() -> u64 {
        time::calibration().map_or(0, |c| c.tsc_hz)
    }

    fn describe_user_mode(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
        user::describe(line);
    }
}

/// Physical address of the crash log region, or 0.
static CRASH_LOG: AtomicU64 = AtomicU64::new(0);
static CRASH_LOG_TAKEN: AtomicBool = AtomicBool::new(false);

fn set_crash_log_region(phys: u64) {
    CRASH_LOG.store(phys, Ordering::Relaxed);
}

/// Logs what the boot CPU is.
fn log_cpu() {
    let mut brand = [0u8; 48];
    let vendor = cpu::vendor();
    let (family, model, stepping) = cpu::family_model_stepping();
    let mut revision = radix_klog::TextBuf::<40>::new();
    if cpu::is_intel() {
        use core::fmt::Write;
        let _ = write!(revision, ", microcode revision {:#x}", microcode::revision());
    }
    radix_klog::info!(
        "CPU: {} ({}, family {family:#x}, model {model:#x}, stepping {stepping}{revision}){}",
        cpu::brand(&mut brand),
        core::str::from_utf8(&vendor).unwrap_or("unknown vendor"),
        if cpu::feature::hypervisor() { ", under a hypervisor" } else { "" }
    );
}

unsafe extern "C" {
    static __ksyms_start: u8;
    static __ksyms_end: u8;
}

impl Platform for Arch {
    fn init(params: &BootParams) {
        let flags = CommandLine::new(params.command_line);
        log_cpu();
        let acpi = match acpi::init(entry::RSDP_COPY.get().copied()) {
            Ok(acpi) => Some(acpi),
            Err(err) => {
                radix_klog::warn!("ACPI: {err}; using one CPU and legacy timers");
                None
            }
        };
        acpi::describe(&mut |line| radix_klog::info!("{line}"));
        let madt = acpi.and_then(|acpi| acpi.madt.as_ref());

        apic::disable_pics();
        apic::init(madt, !flags.has("nox2apic"));
        smp::discover_cpus();
        memory::start_frame_allocator(
            acpi.and_then(|acpi| acpi.srat.as_ref()),
            acpi.and_then(|acpi| acpi.slit.as_ref()),
        );
        if let Some(report) = memory::handover() {
            let nodes = radix_mm::FRAMES.nodes();
            let plural = |count: usize| if count == 1 { "" } else { "s" };
            radix_klog::info!(
                "Memory: frame allocator took {} region{} on {} NUMA node{}, {} MiB free; {} KiB \
                 of frame records",
                report.regions,
                plural(report.regions),
                nodes,
                plural(nodes),
                (report.free_frames * memory::PAGE) >> 20,
                report.record_bytes / 1024
            );
            if report.skipped_bytes > 0 {
                radix_klog::warn!(
                    "Memory: {} MiB of RAM left out: too many regions",
                    report.skipped_bytes >> 20
                );
            }
        }
        apic::init_cpu(madt, smp::acpi_uid(0));
        kvm::init_cpu();
        if let Some(madt) = madt {
            apic::init_ioapics(madt);
        }
        let (version, lvts) = apic::version();
        radix_klog::info!(
            "APIC: {} mode, local APIC at {:#x} (version {version:#x}, {} LVT entries), boot CPU \
             APIC ID {}{}",
            apic::mode_name(),
            apic::phys_base(),
            lvts + 1,
            apic::id(),
            if apic::pv_eoi() { ", paravirtual EOI" } else { "" }
        );
        if kvm::pv_tlb_flush() {
            radix_klog::info!(
                "KVM: steal time and paravirtual TLB flush (shootdowns send no IPI to a virtual \
                 CPU the host has stopped)"
            );
        }
        apic::ioapics(|io| {
            radix_klog::info!(
                "APIC: I/O APIC {} at {:#x}, version {:#x}, {} inputs from GSI {} (all masked)",
                io.id,
                io.address,
                io.version,
                io.inputs,
                io.gsi_base
            );
        });

        time::calibrate(
            acpi.and_then(|acpi| acpi.hpet.as_ref()),
            acpi.and_then(|acpi| acpi.fadt.as_ref()),
        );
        if let Some(c) = time::calibration() {
            radix_klog::info!(
                "Time: TSC at {}.{:03} MHz ({}){}; APIC timer input {}.{:03} MHz; tick {} Hz",
                c.tsc_hz / 1_000_000,
                c.tsc_hz / 1_000 % 1_000,
                if c.tsc_from_cpuid {
                    "from CPUID"
                } else {
                    match c.reference {
                        "HPET" => "measured against the HPET",
                        "ACPI PM timer" => "measured against the ACPI PM timer",
                        _ => "measured against the 8254 PIT",
                    }
                },
                if c.invariant { ", invariant" } else { "" },
                c.apic_timer_hz / 1_000_000,
                c.apic_timer_hz / 1_000 % 1_000,
                time::TICK_HZ
            );
        }
        time::start_tick();
        rtc::init(acpi.and_then(|acpi| acpi.fadt.as_ref()).map_or(0, |fadt| fadt.century));
        // Under a hypervisor MWAIT is usually not passed through, and when
        // it is emulated it may spin; HLT lets the host run something else.
        IDLE_MWAIT.store(cpu::feature::monitor() && !cpu::feature::hypervisor(), Ordering::Relaxed);
    }

    fn current_fault() -> Option<Fault> {
        *percpu::current().fault.lock()
    }

    fn set_breakpoint_handler(handler: fn(&Fault)) {
        interrupts::set_breakpoint_handler(handler);
    }

    fn reset() -> ! {
        power::reset()
    }

    fn power_off() {
        power::power_off();
    }

    fn console_read() -> Option<u8> {
        serial::read()
    }

    fn key_pressed() -> bool {
        power::key_pressed()
    }

    fn take_crash_log_region() -> Option<&'static mut [u8]> {
        let phys = CRASH_LOG.load(Ordering::Relaxed);
        if phys == 0 || CRASH_LOG_TAKEN.swap(true, Ordering::AcqRel) {
            return None;
        }
        let virt = memory::phys_to_virt(phys);
        // SAFETY: the region is usable RAM (so the direct map covers it),
        // it was reserved before the frame allocator started, and the swap
        // above makes this the only reference ever handed out.
        Some(unsafe {
            core::slice::from_raw_parts_mut(virt as *mut u8, memory::CRASH_LOG_SIZE as usize)
        })
    }

    fn emulator_exit(success: bool) {
        power::emulator_exit(success);
    }

    fn describe_firmware(line: &mut dyn FnMut(fmt::Arguments<'_>)) {
        acpi::describe(line);
    }

    fn has_debug_console() -> bool {
        serial::present()
    }

    fn console_write(text: &str) {
        serial::write(text);
    }

    fn kernel_symbols() -> &'static [u8] {
        // SAFETY: the linker script reserves `.ksyms` between these two
        // symbols in the read-only part of the image, which is mapped for
        // the kernel's whole life; `ob build` fills it before booting and
        // nothing writes it afterwards.
        unsafe {
            let start = &raw const __ksyms_start;
            let len = (&raw const __ksyms_end as usize) - (start as usize);
            core::slice::from_raw_parts(start, len)
        }
    }

    #[inline(never)]
    fn self_test_fault(test: FaultTest) -> &'static str {
        radix_klog::tracepoint!("selftest.fault");
        match test {
            FaultTest::Unmapped => {
                // SAFETY: deliberately writes to an address that is never
                // mapped; the page fault handler panics and never returns.
                unsafe {
                    core::arch::asm!(
                        "mov qword ptr [{}], 0x5a",
                        in(reg) 0xdead_0000usize,
                        options(nostack)
                    );
                }
                panic!("self-test fault: the write to an unmapped address did not fault")
            }
            FaultTest::UserCode => {
                if !paging::SMEP.load(Ordering::Relaxed) {
                    return "this CPU has no SMEP";
                }
                let page = paging::map_user_test_page(true);
                // SAFETY: deliberately calls a `ret` instruction in a user
                // page; SMEP makes the fetch fault, and the page fault
                // handler panics. Were SMEP off, it would just return.
                unsafe { core::arch::asm!("call {}", in(reg) page, clobber_abi("C")) };
                panic!(
                    "self-test fault: running code in a user page did not fault (CR4 {:#x}, \
                     entries {:x?})",
                    cpu::cr4(),
                    paging::walk(page)
                )
            }
            FaultTest::UserData => {
                if !paging::SMAP.load(Ordering::Relaxed) {
                    return "this CPU has no SMAP";
                }
                let page = paging::map_user_test_page(false);
                // SAFETY: deliberately reads a user page; SMAP makes the
                // read fault, and the page fault handler panics.
                let value = unsafe { core::ptr::read_volatile(page as *const u64) };
                panic!(
                    "self-test fault: reading a user page did not fault (read {value:#x}; CR4 \
                     {:#x}, entries {:x?})",
                    cpu::cr4(),
                    paging::walk(page)
                )
            }
        }
    }
}
