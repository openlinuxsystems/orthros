radix-debug(7) "OrthrOS" "OrthrOS Manual"

# NAME

radix-debug - panic reports, the crash log, the kernel debugger, GDB and
trace points in the Radix kernel

# DESCRIPTION

Radix has to be debuggable on its reference laptop, which has no serial
port. So a panic fills the screen with its report, and the console output
is also kept in memory that survives a warm restart, where the next boot
finds it. With a serial port, or under QEMU, there is also *kdb*, a small
kernel debugger, and GDB through QEMU's GDB stub. Trace points record
frequent events cheaply, from the first Rust function on.

This page describes all of them as of Phase 1. The kernel flags named here
are listed in *orthros-boot*(7).

# PANIC REPORT

The kernel panics when Rust code calls *panic!* or fails an assertion, and
when the CPU raises an exception the kernel cannot handle, such as a page
fault, a general protection fault or a double fault. The panicking CPU
then:

. stops the other CPUs with a non-maskable interrupt;
. writes the report to the serial port, the VGA text screen if in use, and
  the crash log;
. fills the framebuffer with the report under a red *RADIX KERNEL PANIC*
  banner, followed by as many recent log messages as fit;
. records in the crash log that this boot ended with a panic;
. under QEMU with the *qemu-exit* flag, exits QEMU with status 35;
. with the *kdb* flag, runs the kernel debugger on the serial port;
. waits for a key on the built-in keyboard or the serial port, or for the
  delay given by *panic*=_seconds_, and restarts the machine.

A report looks like this:

```
Radix panic on CPU 0 at 0.327298 s: Page fault (#PF) at 0xffffffff8011416e while writing 0xdead0000: the page is not mapped
  Radix 0.0.1 on x86_64, boot 1; 1 of 1 other CPUs stopped
Registers:
     rax 00000000dead0000     rbx 0000000000000000     rcx 00000000f6d8320b     rdx 000000001fcbfbfd
     ...
     rip ffffffff8011416e  rflags 0000000000000246      cs 0000000000000008      ss 0000000000000010
     cr2 00000000dead0000     cr3 0000000000331000     cr4 00000000000000a0   error 0000000000000002
Backtrace:
  #0  0xffffffff8011416e  <radix_arch_x86_64::Arch as radix_hal::Platform>::self_test_fault+0x3e
  #1  0xffffffff8010dedf  radix::selftest::page_fault+0x1f
  #2  0xffffffff8010e1aa  radix::selftest::run+0xca
  #3  0xffffffff8010d496  radix_kernel_main+0x1476
  #4  0xffffffff80113221  radix_arch_entry+0x1091
Press a key to restart; the log is kept in memory for the next boot.
```

The first line gives the CPU, the time since boot and the message. For an
exception the message names it and its address; a page fault also says
which address was accessed, how, and why it failed, and points out an
overflow into a stack guard page. A panic from Rust code is followed by
the source location (*at* _file_:_line_:_column_). The second line names
the kernel version, the architecture, the boot number from the crash log
and how many other CPUs stopped.

*Registers* appear after an exception: the general-purpose registers, the
instruction pointer and flags, the code and stack segments, CR2 (the
faulting address of a page fault), CR3, CR4 and the exception's error
code.

*Backtrace* lists the functions on the stack, innermost first, found by
following the frame pointers (the kernel and its copy of *core* keep them).
Frame #0 is the faulting instruction after an exception; the others are
return addresses. Each shows _function_+_offset_ from the kernel's symbol
table (see *SYMBOL TABLE*), *?* for an address outside any known
function, or a note that the image has no symbol table. At most 32 frames
are shown.

The restart keeps memory: the kernel tries the reset register named in
the ACPI FADT, then the keyboard controller's reset line, then the chipset
reset control register (port 0xCF9), then a triple fault.

# CRASH LOG

Everything the consoles print, debug messages included, is also written to
1 MiB of RAM reserved at the same physical address on every boot: the
first 2 MiB boundary above 4 GiB where there is RAM, or else 64 MiB. A warm
restart (a panic, *reboot* in *kdb*, the reset button on many machines)
leaves RAM as it was, so the next boot finds the log there and prints how
the previous boot ended and its last lines: 40 after a panic, 12
otherwise. For example:

```
Crash log: boot 2; the previous boot (1) ended with a kernel panic. Its last 40 lines:
  | [    0.298033] selftest page-fault: touching an unmapped address; a page-fault panic follows
  | ...
```

A boot that hung, or was reset by the user, reads as "stopped while
running". Powering the machine off loses the log, and firmware that tests
or clears memory at start-up can overwrite it. *crashlog*=off turns the
log off.

## Format

The region is two equal slots. Each boot writes the slot the previous boot
did not use, so the previous log survives until the boot after next. A
slot is a 64-byte header followed by a ring of text. All integers are
little-endian:

[[ *Offset*
:- *Size*
:< *Contents*
|  0
:  8
:  Magic, the ASCII characters RADIXLOG
|  8
:  4
:  Format version, 1
|  12
:  4
:  Boot number, counting from 1
|  16
:  4
:  How the boot ended: 1 still running (hang, reset or power loss), 2 kernel panic, 3 requested restart
|  20
:  4
:  Size of the ring in bytes
|  24
:  8
:  Total bytes written to the ring; when larger than the ring, the oldest byte is at this value modulo the ring size
|  32
:  4
:  FNV-1a hash of bytes 0 to 31
|  36
:  28
:  Zero

A slot whose magic, version, ring size or hash does not match is ignored.

# KERNEL DEBUGGER

*kdb* is a command line on the serial port, for use under QEMU (where
*ob run* connects the serial port to the terminal) or on machines with a
serial port. Boot with the *kdb* flag, for example *./ob run --cmdline
kdb*. The kernel then enters *kdb*:

- at the end of boot, after *Boot complete.*;
- at a breakpoint instruction (*int3*), with the registers it saved;
- after a panic, with the panic's registers and backtrace.

Only the CPU that entered *kdb* stops; the others keep running, except
after a panic. Without a serial port *kdb* says so in the log and does not
run.

Lines are edited with Backspace, Ctrl-U (erase the line) and Ctrl-C
(cancel the line); the Up arrow recalls the previous command. Addresses
are hexadecimal, with or without *0x*; counts are decimal, or hexadecimal
with *0x*. Both may contain *\_* separators, as in *ffffffff_80100000*.

*help*
	List the commands.

*log* [_count_]
	Show the last _count_ log messages, 20 by default, debug messages
	included.

*trace* [*on*|*off*]
	Show the recorded trace points, or start or stop recording. See
	*TRACE POINTS*.

*cpus*
	List the CPUs with their APIC IDs, state, timer interrupts and
	inter-processor interrupts received.

*mem*
	Show the firmware's memory map and the kernel's reservations.

*acpi*
	List the ACPI tables and what the kernel read from them.

*bt*
	Show the backtrace where *kdb* was entered: the panic's or the
	breakpoint's, or *kdb*'s own at the end of boot.

*regs*
	Show the registers saved by the breakpoint or exception that entered
	*kdb*.

*x* _address_ [_length_]
	Dump _length_ bytes (64 by default, at most 4096) at _address_ in hex
	and ASCII. Only memory the kernel has mapped and that is RAM can be
	read, so the dump cannot touch device registers or fault.

*sym* _address_|_name_
	Show the function containing _address_, as _function_+_offset_, or the
	address of the function called _name_.

*panic*
	Panic deliberately, to try the panic path. *kdb* runs again after the
	report.

*reboot*
	Restart the machine, recording a requested restart in the crash log.

*continue*
	Leave *kdb* (also *c*). The kernel carries on; after a panic it
	restarts as the report says.

A session looks like this:

```
kdb on CPU 0, boot complete. Type `help` for commands, `continue` to leave.
kdb> sym radix_kernel_main
radix_kernel_main is at 0xffffffff8010c020
kdb> x 0xffffffff8010c020 16
ffffffff8010c020  55 48 89 e5 41 57 41 56  41 55 41 54 53 48 81 ec  |UH..AWAVAUATSH..|
kdb> continue
kdb: continuing
```

# SELF-TESTS

*selftest*=_name_ runs one of these at the end of boot, before *Boot
complete.* *ob test* boots each of them.

*page-fault*
	Writes to an unmapped address. The resulting panic report must name
	the page fault, show the registers and give a backtrace through the
	self-test's functions.

*crash-reboot*
	Panics on purpose. The next boot finds the panic in the crash log,
	reports *selftest crash-reboot: passed* and boots normally. Under QEMU
	this needs a restart that keeps RAM, so *ob test* runs it without
	QEMU's *-no-reboot* and with *panic=0*.

*breakpoint*
	Runs a breakpoint instruction. With *kdb* the debugger stops there;
	without it the breakpoint is logged and skipped. Either way the
	kernel must carry on after it.

# TRACE POINTS

A trace point is a named place in the kernel code that records an event
when tracing is on: the time, the CPU, the trace point and up to three
numbers. Events go into a ring of 1024 entries without taking a lock, so
trace points work in interrupt handlers and from the first Rust function,
before anything is set up. When tracing is off a trace point costs one
memory read.

Boot with the *trace* flag to record from the start of *radix_arch_entry*,
or type *trace on* in *kdb*; *trace* in *kdb* prints the ring, oldest
first. Times before timer calibration show as 0. Phase 1 has these trace
points:

[[ *Name*
:[ *Numbers*
:< *Event*
|  boot.entry
:  magic, boot information address
:  First Rust function entered
|  boot.memory
:  memory map entries
:  Memory reserved, allocator ready
|  boot.paging
:  -
:  Kernel page tables active
|  boot.kernel-main
:  -
:  *radix_kernel_main* called
|  timer.start
:  CPU, initial count
:  Periodic timer started on a CPU
|  smp.start
:  CPU, APIC ID
:  Start-up IPIs sent to a CPU
|  smp.online
:  CPU, APIC ID
:  A started CPU reached Rust code
|  ipi.ping
:  CPU
:  Ping IPI received
|  apic.error
:  error status
:  Local APIC error interrupt
|  pic.spurious
:  vector
:  Spurious interrupt from the masked 8259 PICs
|  irq.unexpected
:  vector
:  Interrupt on a vector with no handler
|  nmi.unexpected
:  instruction address
:  NMI outside the panic path
|  selftest.fault
:  -
:  The *page-fault* self-test is about to fault

# DEBUGGING WITH GDB

*./ob run --gdb* starts QEMU with its GDB stub on localhost port 1234,
paused before the firmware runs, and writes _out/qemu/radix.gdb_. In a
second terminal run

```
gdb -x out/qemu/radix.gdb
```

The script loads the kernel's symbols from _out/sysroot/boot/radix_,
connects to QEMU, sets a hardware breakpoint on *radix_arch_entry*, the
first Rust function, and continues. A hardware breakpoint is used because
the firmware and GRUB overwrite memory, which removes software
breakpoints set before the kernel is loaded. From there the usual GDB
commands work: *bt*, *info registers*, *break* _function_, *next*,
*print*. Add *--debug* to *ob run* for an unoptimised kernel that steps
line by line.

GDB must support x86-64; on Debian and Ubuntu the *gdb* package does.
*ob doctor* reports whether it is installed.

# SYMBOL TABLE

The kernel image reserves a 1 MiB section, *.ksyms*, for its own symbol
table, so a panic can name functions without reading debug information.
After linking, *ob build* reads the function symbols from the ELF symbol
table, demangles them, and writes them into the section in place; nothing
else in the image moves. A kernel linked without *ob* has an empty table,
and its backtraces show bare addresses. The table holds symbols in the
kernel's higher-half text only.

All integers are little-endian:

[[ *Offset*
:- *Size*
:< *Contents*
|  0
:  8
:  Magic, the ASCII characters RADIXSYM
|  8
:  4
:  Format version, 1
|  12
:  4
:  Number of symbols, _n_
|  16
:  8
:  Base address that symbol offsets are relative to
|  24
:  4
:  Length of the whole table in bytes
|  28
:  4
:  Zero
|  32
:  16 _n_
:  Symbols sorted by address, each four 32-bit fields: offset from the base, size in bytes, offset of the name from the start of the table, length of the name
|  32 + 16 _n_
:  -
:  The names, UTF-8, not terminated

# FILES

_out/sysroot/boot/radix_
	The kernel built by *ob build*, with its symbol table.

_out/qemu/radix.gdb_
	The GDB script written by *ob run --gdb*.

_out/test/_
	Serial logs and screenshots of the boot tests run by *ob test*.

# SEE ALSO

*orthros-boot*(7), *ob*(1), *gdb*(1), *qemu-system-x86_64*(1)
