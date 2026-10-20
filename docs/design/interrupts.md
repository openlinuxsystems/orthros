# Interrupts, timers and SMP

This note describes how the x86-64 layer of Radix handles exceptions and
interrupts, keeps time and starts the other CPUs, as of Phase 1.
`boot.md` covers what happens before. The code is in
`kernel/arch/x86_64/src/`: `percpu.rs`, `interrupts.rs`, `apic.rs`,
`time.rs` and `smp.rs`.

## Per-CPU state

Each CPU has a `PerCpu` block, found through the `GS` base (`gs:[8]` is the
CPU's index), holding its APIC ID, state (present, online, stopped), GDT,
TSS, stacks, counters (timer ticks, pings received) and the fault it is
panicking for. The
GDT has a null descriptor, 64-bit kernel code (0x08) and data (0x10), and
the TSS. The TSS names three interrupt stacks (IST), each with a guard
page:

| IST | Used by | Why |
| --- | --- | --- |
| 1 | double fault (#DF) | a kernel stack overflow raises #DF; it needs a good stack |
| 2 | NMI | NMIs can arrive at any instruction, including during entry |
| 3 | machine check (#MC) | as for NMIs |

Phase 1 has no user mode, so the TSS's ring-0 stack is unused.

## Entry stubs and dispatch

The IDT has 256 interrupt gates. Each vector enters through a small
assembly stub that pushes a zero error code where the CPU pushes none, then
the vector number, and jumps to a common stub. The common stub saves every
general-purpose register into a `TrapFrame`, pushes a fake frame record
(interrupted RIP and RBP) so that a frame-pointer backtrace taken inside a
handler continues into the interrupted code, and calls `dispatch` with the
frame. On return it restores the registers and executes `iretq`.

`dispatch` handles:

| Vector | Source | Handling |
| --- | --- | --- |
| 0-31 | CPU exceptions | fatal, see below; #BP and NMI are special |
| 0x20-0x2F | the 8259 PICs, remapped | spurious only (all masked); counted as a trace point |
| 0x30 | local APIC timer | counts a tick on this CPU, EOI |
| 0x40 | ping IPI | counts a ping, EOI |
| 0x41 | TLB flush IPI | flushes the TLB if a mapping was removed since this CPU last did; not sent until Phase 2 |
| 0xFE | local APIC error | reads and clears the error status, trace point |
| 0xFF | local APIC spurious | ignored, no EOI |
| others | nothing should send them | trace point `irq.unexpected`, EOI |

## Exceptions

Every exception in kernel code is fatal except two:

- **Breakpoint (#BP, `int3`)** calls the handler registered with
  `set_breakpoint_handler` (`kdb` when booted with `kdb`) and returns to
  the instruction after the breakpoint.
- **NMI** halts the CPU when the panic path is stopping CPUs (`STOPPING`).
  Otherwise it is recorded as `nmi.unexpected` and ignored.

A fatal exception builds a `Fault` (all registers, CR2, CR3, CR4, the error
code), stores it for the panic handler, and panics with a message naming
the exception and the instruction address. Page faults also say what was
accessed and how, and whether the address is in a stack guard page. The
panic handler takes the backtrace from the faulting frame, so it starts at
the faulting instruction rather than in the handler.

## Interrupt controllers

`apic::init` runs once on the boot CPU, after the ACPI tables are read:

1. **8259 PICs**: remapped to vectors 0x20-0x2F (away from the exceptions)
   and fully masked. Interrupts from them can still arrive as spurious
   IRQ 7 or 15.
2. **Local APIC mode**: x2APIC (registers as MSRs, 32-bit APIC IDs) when
   CPUID reports it and `nox2apic` is not given; xAPIC (registers in a
   4 KiB page mapped uncached) otherwise. If the firmware already enabled
   x2APIC, the kernel stays in it, because leaving x2APIC requires
   disabling the APIC entirely.
3. **I/O APICs** from the MADT: mapped, and every redirection entry masked.
   Phase 1 takes no device interrupts; the MADT's interrupt source
   overrides, which later phases need to route ISA interrupts, are parsed
   and counted in the boot log.

Each CPU then enables its local APIC (`init_cpu`): spurious vector 0xFF,
task priority 0, error vector 0xFE, and LINT0/LINT1 programmed from the
MADT's local APIC NMI entries for that CPU's ACPI processor UID.

In x2APIC mode, writing the ICR MSR is not serializing, so the kernel
issues `mfence; lfence` before it, as the SDM requires, to make earlier
memory writes visible to the target CPU before the IPI arrives.

## Time

The kernel's clock is the TSC, counted from the first Rust instruction and
converted to nanoseconds with a 32.32 fixed-point factor. Calibration
(`time::calibrate`) picks the best reference the machine has:

1. the HPET, from the ACPI HPET table;
2. the ACPI PM timer, from the FADT;
3. the 8254 PIT channel 2, gated through port 0x61.

It measures the TSC and the local APIC timer (divide by 16) against the
reference over 50 ms. CPUID leaf 0x15 can give the TSC rate exactly; the
kernel uses it only when it agrees with the measurement within 2%, because
some hypervisors and firmware report values that are wrong. The boot log
says which source was used and whether the TSC is invariant.

Each CPU then runs its local APIC timer in periodic mode at 100 Hz
(`TICK_HZ`) on vector 0x30. `radix_kernel_main` waits up to two seconds for
every online CPU to count three ticks, and fails loudly if one does not.

Before calibration, `delay_us` spins on port 0x80 writes (about 1 µs each
on PC hardware), and log times are 0.

## Inter-processor interrupts

`send_ipi(cpu, Ipi::Ping)` sends a fixed IPI to one CPU by APIC ID; the
target counts it. At boot the kernel pings every other CPU in turn and
waits for each count to change, which proves that IPIs and each CPU's
interrupt handling work. The panic path uses an NMI to all other CPUs
(`stop_others`) and waits up to 100 ms for them to halt.

## Starting the other CPUs

`smp::discover_cpus` takes the enabled processors from the MADT, skipping
an APIC ID listed twice (as xAPIC and x2APIC entries), and IDs above 0xFE
unless the APIC is in x2APIC mode, since xAPIC IPIs cannot reach them.

`start_secondary_cpus` copies the real-mode trampoline
(`ap_trampoline_start` in `smp.rs`) to a free page below 1 MiB (0x8000
when usable), maps that page at its physical address in the kernel's
tables while CPUs start, and patches in the kernel's CR3, whether NX is
available, and the 64-bit entry address. Then, for each CPU in turn, it:

1. allocates the CPU's kernel stack and three IST stacks, each with a
   guard page, in the mapping window, and patches the stack top and the
   CPU's index into the trampoline;
2. sends INIT, waits 10 ms, and sends a start-up IPI with the trampoline's
   page number;
3. waits up to a second for the CPU to report in, then sends a second
   start-up IPI and waits two more seconds, and logs a CPU that never
   starts.

Afterwards the trampoline page is unmapped again.

The new CPU switches from 16-bit real mode to protected mode and long mode
on the kernel's page tables, and jumps to `ap_entry` in the higher half,
which loads its GDT, TSS and IDT, applies microcode, enables its local
APIC and timer, marks itself online and calls the kernel's entry for
secondary CPUs, which idles. CPUs start one at a time because they share
the trampoline's parameter block.

## Locks and interrupts

`radix-sync`'s `SpinLock` does not disable interrupts. The interrupt
handlers of Phase 1 (timer, IPIs, APIC errors) take no locks: they only
touch atomics and the lock-free trace ring. The handlers that do take locks
are for exceptions, which interrupt gates enter with interrupts off: `kdb`
at a breakpoint, and the panic path. The log takes its lock with
interrupts disabled, so a timer tick cannot interrupt a CPU that holds it,
and the panic path's panic mode takes over locks a stopped CPU held.
Phase 2 adds interrupt-safe locks for device interrupts.
