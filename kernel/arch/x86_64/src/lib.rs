//! x86-64 support for Radix: the implementation of the hardware abstraction
//! layer on PC hardware.
//!
//! This crate owns the machine from GRUB's jump to `_start` until it calls
//! the kernel's `radix_kernel_main`: the boot trampoline ([`boot`]), the
//! CPU's descriptor tables ([`percpu`], [`interrupts`]), physical memory and
//! the kernel's page tables ([`memory`], [`paging`]), the consoles it knows
//! about ([`serial`], [`vga`]) and early microcode ([`microcode`]). Later,
//! through [`Platform::init`], it reads the ACPI tables ([`acpi`]), sets up
//! the interrupt controllers ([`apic`]) and timers ([`time`]), and it starts
//! and stops the other CPUs ([`smp`]).
//!
//! It is part of the trusted computing base, so it may use `unsafe`, and
//! every `unsafe` block states why it is sound. docs/design/boot.md and
//! docs/design/interrupts.md describe the design.

#![no_std]

mod acpi;
mod apic;
mod boot;
mod cpu;
mod entry;
mod interrupts;
mod io;
mod kaslr;
mod memory;
mod microcode;
mod paging;
mod percpu;
mod power;
mod serial;
mod smp;
mod space;
mod time;
mod vga;

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radix_hal::cmdline::CommandLine;
use radix_hal::{
    BootParams, Cpu, CpuInfo, CpuLocal, Fault, FaultTest, InterruptController, Ipi, PageTable,
    Platform, Smp, Timer,
};

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
        let Some(stacks) = percpu::current().stacks.get() else { return 0 };
        let mut fp = frame_pointer;
        let mut count = 0;
        while count < out.len() {
            let Some(stack) = stacks.containing(fp) else { break };
            if !fp.is_multiple_of(8) || fp + 16 > stack.end {
                break;
            }
            // SAFETY: `fp` and `fp + 8` lie inside one of this CPU's kernel
            // stacks (checked above), which are mapped for the kernel's
            // whole life; volatile reads of them have no side effects.
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
            if next <= fp && stacks.containing(next) == Some(stack.clone()) {
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
}

impl InterruptController for Arch {
    fn name() -> &'static str {
        apic::mode_name()
    }

    fn send_ipi(cpu: usize, ipi: Ipi) {
        let Some(target) = percpu::get(cpu) else { return };
        match ipi {
            Ipi::Ping => {
                apic::send_fixed(target.apic_id.load(Ordering::Relaxed), interrupts::VECTOR_PING)
            }
        }
    }

    fn pings_received(cpu: usize) -> u64 {
        percpu::get(cpu).map_or(0, |c| c.pings.load(Ordering::Relaxed))
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
        if let Some(madt) = madt {
            apic::init_ioapics(madt);
        }
        let (version, lvts) = apic::version();
        radix_klog::info!(
            "APIC: {} mode, local APIC at {:#x} (version {version:#x}, {} LVT entries), boot CPU \
             APIC ID {}",
            apic::mode_name(),
            apic::phys_base(),
            lvts + 1,
            apic::id()
        );
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
