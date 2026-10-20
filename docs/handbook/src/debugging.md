# Debugging the kernel

This chapter shows how to find out what went wrong when Radix crashes or
misbehaves, in QEMU or on a real machine. The reference is
`radix-debug(7)`; the kernel flags are in `orthros-boot(7)`.

## Reading a panic report

When the kernel panics, the screen turns into a report under a red
**RADIX KERNEL PANIC** banner, and the same report goes to the serial port:

```text
Radix panic on CPU 0 at 0.327298 s: Page fault (#PF) at 0xffffffff8011416e while writing 0xdead0000: the page is not mapped
  Radix 0.0.1 on x86_64, boot 1; 1 of 1 other CPUs stopped
Registers:
     rax 00000000dead0000     rbx 0000000000000000     rcx 00000000f6d8320b     rdx 000000001fcbfbfd
     ...
Backtrace:
  #0  0xffffffff8011416e  <radix_arch_x86_64::Arch as radix_hal::Platform>::self_test_fault+0x3e
  #1  0xffffffff8010dedf  radix::selftest::page_fault+0x1f
  #2  0xffffffff8010e1aa  radix::selftest::run+0xca
  #3  0xffffffff8010d496  radix_kernel_main+0x1476
  #4  0xffffffff80113221  radix_arch_entry+0x1091
Press a key to restart; the log is kept in memory for the next boot.
```

Read it from the top:

- The first line says what happened. For a CPU exception it names the
  exception, the instruction that caused it and, for a page fault, the
  address that was accessed and why that failed. A Rust panic shows its
  message and the source line on the next line.
- **Registers** (after exceptions only) are the CPU state at the fault.
  `cr2` is the faulting address of a page fault.
- **Backtrace** lists the functions on the stack, innermost first. Frame
  #0 is where the fault happened; each later frame is the function that
  called the one above it.

Below the report the screen shows the most recent log messages, which often
explain what the kernel was doing. Press a key to restart. To photograph
the screen at leisure, that is all the time you need; to restart
automatically instead, boot with `panic=10` (ten seconds).

## After a crash: the crash log

The kernel keeps a copy of everything it prints in a reserved part of
memory that a restart does not clear. After a panic, a press of a reset
button, or a `reboot` in the debugger, the next boot starts with:

```text
Crash log: boot 2; the previous boot (1) ended with a kernel panic. Its last 40 lines:
  | [    0.298033] selftest page-fault: touching an unmapped address; a page-fault panic follows
  | ...
```

This is the main tool on the Dell Inspiron 15 5570, which has no serial
port: a panic restarts into a boot that shows what led up to it. Switching
the machine off, rather than restarting it, loses the log. That makes a
hang on a laptop without a reset button the hard case, since holding the
power button cuts power; when the screen freezes, photograph it, and try
again with `loglevel=debug` to see more of what happened before.

## The kernel debugger

On QEMU, `kdb` gives a command line on the serial port, which `./ob run`
connects to your terminal:

```sh
./ob run --cmdline kdb
```

After `Boot complete.` the prompt appears:

```text
kdb on CPU 0, boot complete. Type `help` for commands, `continue` to leave.
kdb> cpus
4 CPUs, interrupts by xAPIC, timer at 100 Hz:
  cpu0: APIC ID 0, boot CPU, online, 24 timer interrupts, 0 pings
  ...
kdb> sym 0xffffffff8010dedf
0xffffffff8010dedf is radix::selftest::page_fault+0x1f
```

`help` lists the commands: the log, trace points, CPUs, the memory map,
ACPI tables, backtraces, registers, memory dumps and symbol lookups.
`kdb` also stops at breakpoints and after a panic. `continue` leaves it.

## Debugging with GDB

For stepping through code, use QEMU's GDB stub:

```sh
./ob run --gdb --debug        # terminal 1: QEMU waits for GDB
gdb -x out/qemu/radix.gdb     # terminal 2
```

GDB stops at `radix_arch_entry`, the kernel's first Rust function, with the
kernel's symbols loaded. From there `break`, `next`, `step`, `bt` and
`print` work as usual; `--debug` builds the kernel without optimisation so
stepping follows the source.

## Trace points

Some events, such as interrupts, happen too often to log. Trace points
record them cheaply in a ring of 1024 events. Boot with `trace` to record
from the very start, then type `trace` in `kdb` to see them:

```sh
./ob run --cmdline "kdb trace"
```

## Reporting a crash

When you report a kernel crash in an issue, include:

- the whole panic report (a photo of the screen is fine) and, if the
  machine restarted, the crash log lines from the next boot;
- the output of `git rev-parse HEAD` for the source you built;
- the machine, and whether it booted through UEFI or legacy BIOS (the
  `Firmware:` line at the top of the boot log);
- the kernel flags used, from the `Command line:` line.
