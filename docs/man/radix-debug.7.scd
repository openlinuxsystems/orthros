radix-debug(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-debug - panic reports, the crash log, the kernel debugger, the lock
checker, GDB and trace points in the Radix kernel

# DESCRIPTION

Radix has to be debuggable on its reference laptop, which has no serial
port. So a panic fills the screen with its report, and the console output
is also kept in memory that survives a warm restart, where the next boot
finds it. With a serial port, or under QEMU, there is also *kdb*, a small
kernel debugger, and GDB through QEMU's GDB stub. Without one, a second
computer can read the console and talk to *kdb* over the USB controller's
Debug Capability (see *DEBUG CAPABILITY CONSOLE*). Trace points record
frequent events cheaply, from the first Rust function on. The lock
checker reports locking bugs the first time the code that has them runs,
long before they deadlock.

This page describes all of them as of Phase 4. The kernel flags named here
are listed in *dukovo-boot*(7).

# PANIC REPORT

The kernel panics when Rust code calls *panic!* or fails an assertion,
when the CPU raises an exception the kernel cannot handle, such as a page
fault, a general protection fault or a double fault, and when the heap
finds one of its objects damaged (see *HEAP CORRUPTION*). The panicking
CPU then:

. stops the other CPUs with a non-maskable interrupt;
. writes the report to the serial port, the VGA text screen if in use, the
  Debug Capability console with the *dbc* flag (giving it up to 2 s to go
  out), and the crash log;
. fills the framebuffer with the report under a red *RADIX KERNEL PANIC*
  banner, followed by as many recent log messages as fit;
. records in the crash log that this boot ended with a panic;
. under QEMU with the *qemu-exit* flag, exits QEMU with status 35;
. with the *kdb* flag, runs the kernel debugger on the debug console;
. waits for a key on the built-in keyboard or the debug console, or for the
  delay given by *panic*=_seconds_, and restarts the machine.

A report looks like this:

```
Radix panic on CPU 0 at 0.327298 s: Page fault (#PF) at 0xffffffffa031416e while writing 0xdead0000: the page is not mapped
  Radix 0.10.4 on x86_64, boot 1; 1 of 1 other CPUs stopped
Registers:
     rax 00000000dead0000     rbx 0000000000000000     rcx 00000000f6d8320b     rdx 000000001fcbfbfd
     ...
     rip ffffffffa031416e  rflags 0000000000000246      cs 0000000000000008      ss 0000000000000010
     cr2 00000000dead0000     cr3 0000000000360000     cr4 00000000003000a0   error 0000000000000002
Backtrace:
  #0  0xffffffffa031416e  <radix_arch_x86_64::Arch as radix_hal::Platform>::self_test_fault+0x3e
  #1  0xffffffffa030dedf  radix::selftest::fault+0x5f
  #2  0xffffffffa030e1aa  radix::selftest::run+0xca
  #3  0xffffffffa030d496  radix_kernel_main+0x1476
  #4  0xffffffffa0313221  radix_arch_entry+0x1091
Press a key to restart; the log is kept in memory for the next boot.
```

The first line gives the CPU, the time since boot and the message. For an
exception the message names it and its address; a page fault also says
which address was accessed, how, and why it failed. It points out an
overflow into a stack guard page (a stack overflow usually ends in a
double fault, whose message then names the guard page), and a fault that
SMEP or SMAP caused because the kernel ran code in, or touched, a page of
the lower, user half. A panic from Rust code is followed by the source
location (*at* _file_:_line_:_column_). The second line names the kernel
version, the architecture, the boot number from the crash log and how
many other CPUs stopped.

Addresses in the report are where the code ran. With KASLR on, the
kernel is somewhere else on every boot (see *dukovo-boot*(7)); the
boot log's *Layout* line says where, and the backtrace's function names
are found by subtracting the slide.

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

# HEAP CORRUPTION

The kernel heap fills the first two words of every free object with a
marker, the index of the next free object and a poison pattern, and
checks them when it hands the object out again. A mismatch means the
memory was written after it was freed, or the slab's list of free objects
was damaged. A free is also checked: freeing an object twice, or freeing
an address that is not the start of an object, is caught there. The heap
then stops using the object (or the whole slab) and the kernel panics
with a message such as

```
heap corruption: a write after free to the 64-byte object at 0xffff962340a1c040
```

whose backtrace shows the allocation or free that found the damage, not
the code that caused it. *kdb*'s *heap* command counts the damaged
objects found so far.

# CRASH LOG

Everything the consoles print, debug messages included, is also written to
1 MiB of RAM reserved at the same physical address on every boot: the
first 2 MiB boundary above 4 GiB where there is RAM, or else the first one
from 64 MiB, in either case clear of the kernel image and the boot
modules. A warm
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

*kdb* is a command line on the debug console: the serial port, for use
under QEMU (where *ob run* connects the serial port to the terminal) or
on machines with one, and with the *dbc* flag the Debug Capability
console too (see *DEBUG CAPABILITY CONSOLE*). Boot with the *kdb* flag,
for example *./ob run --cmdline kdb*. The kernel then enters *kdb*:

- at the end of boot, after *Boot complete.*;
- at a breakpoint instruction (*int3*), with the registers it saved;
- after a panic, with the panic's registers and backtrace.

Only the CPU that entered *kdb* stops; the others keep running, except
after a panic. Without a debug console *kdb* says so in the log and does
not run; with the *dbc* flag it first waits up to 5 s at the end of boot
for the USB controllers to look for a Debug Capability. While it waits for a command, *kdb* pets a running hardware watchdog
(*watchdog*(8)) about once a second, so that the machine does not restart
under it.

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
	Show the firmware's memory map, the kernel's reservations, the frame
	allocator's zones with their free memory and reserves, and the
	address-space layout (KASLR, W^X, SMEP, SMAP and PCIDs), as at boot.

*heap*
	Show the kernel heap's totals (objects in use, objects cached in the
	per-CPU magazines, memory in slabs and in large blocks, failed
	requests, damaged objects found) and, for each size class with
	slabs, its slabs and objects.

*caps*
	Count the capability spaces and their slots, the capabilities and
	the kernel objects, in all and by object kind, with the pages held by
	memory objects, the deepest derivation and the revokes under way (see
	*radix-capabilities*(7)). If another CPU holds the capability store's
	lock, it says so instead of waiting.

*threads*
	List every thread: its number, name, CPU, state (and, when
	blocked, what for), scheduling policy, the real-time priority lent
	to it through priority inheritance, the CPUs it may run on, its run
	time, switches, moves between CPUs, and its resource domain. See
	*radix-scheduling*(7).

*sched*
	Show the scheduler's totals (threads, waiters in the wait table,
	wake-ups, mutex handoffs, priority loans, RCU grace periods and
	their average length) and each CPU's run queue: how many threads
	are queued, which one runs, switches, preemptions, time idle, the
	threads pulled from other CPUs at each topology level, and the
	queued threads with their weights or priorities.

*domains*
	List the resource domains as a tree: class, CPU weight, memory used
	and its limit in pages, peak use, charges refused, threads and
	out-of-memory stops; then what the out-of-memory policy has done.

*locks*
	Show whether the lock checker is on, the lock classes it has seen
	(where each was created, and whether it is taken in interrupt
	handlers or with interrupts on), the lock-order edges and the
	problems reported. See *LOCK CHECKER*.

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
	address of the function called _name_. Addresses are run-time
	addresses, with the KASLR slide included.

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

# DEBUG CAPABILITY CONSOLE

Most xHCI USB controllers of the last decade, Intel's and AMD's, have a
Debug Capability: switched on, it makes the controller a USB device on
the first of its USB 3 ports that a second computer, the *debug host*, is
plugged into (ADR 0160). With the *dbc* flag the first controller that
has one switches it on, and the console goes there as well as to the
serial port and the screen: the kernel log (the last 64 KiB of it once
the controller runs, then everything after), a panic report, and *kdb*.

It needs a USB 3 A-to-A *debug cable*, whose data lines cross and whose
power line is cut: a plain A-to-A cable joins the two computers' power,
which neither expects. Plug it into a USB 3 port of each. A Linux
debug host sees a serial port, as it does for Linux's own Debug
Capability (vendor and product 1d6b:0010):

	# modprobe usb_debug
	# screen /dev/ttyUSB0

The log says when a debug host connects, configures the device, goes
away or resets its port; one that has not configured it within 5 s has
it switched off and on again. Under QEMU, whose xHCI controller has no
Debug Capability, the flag only logs that there is none.

# LOCK CHECKER

The lock checker (*radix-lockdep*) watches every spin lock and mutex the
kernel takes and keeps a graph of which lock classes were taken while
holding which. A class is every lock created by one line of code. It
reports, as soon as the code runs:

- two classes taken in both orders, a deadlock waiting for the wrong
  interleaving (*possible deadlock*);
- a class taken while one of the same class is held (*recursion*);
- a lock taken in an interrupt handler that is also taken with
  interrupts on, directly or through a chain of other locks;
- a mutex taken, or a thread switched out, while sleeping is not
  allowed: with a spin lock held, with interrupts off or in an interrupt
  handler (*sleeping where it is not allowed*).

It is on by default in debug builds and off in release builds; the
*lockdep* kernel flag (*dukovo-boot*(7)) overrides either. *ob test*
boots *bios-usb* and every self-test since Phase 4 with it on. A report is
logged at the error level, each line starting *Lock checker:*, with the
places where the locks were created and taken:

```
cpu1: Lock checker: possible deadlock: two locks are taken in both orders (CPU 1)
cpu1: Lock checker:   taking spin lock created at radix/src/schedtest.rs:817:30, taken at radix/src/schedtest.rs:835:30
cpu1: Lock checker:   while holding spin lock created at radix/src/schedtest.rs:818:30, taken at radix/src/schedtest.rs:834:30
cpu1: Lock checker:   the other order was seen before: the lock being taken now at radix/src/schedtest.rs:828:30, then the held one at radix/src/schedtest.rs:829:30
```

After its first report the checker turns itself off, since later reports
may only follow from the first. Its tables are fixed (512 classes, 32
locks held per CPU); running out of room is reported once and also turns
it off. *kdb*'s *locks* command shows its state.

# SELF-TESTS

*selftest*=_name_ runs one of these at the end of boot, before *Boot
complete.* *ob test* boots each of them but *bench*, which *ob bench*
boots.

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

*mm-stress*
	Stresses the memory manager on every CPU at once: each runs 12.5
	million random heap operations (sizes from 1 byte to 64 KiB, both
	allocation contexts, zeroed and not), filling every object with a
	pattern and checking it before the free, then maps 200 areas in the
	mapping window, checks they read as zero, and unmaps them, which
	shoots each down on every CPU. With 4 CPUs that is over 20 million
	objects allocated and freed. It then checks that every frame is back
	in the frame allocator, that the free lists are consistent and that
	no object was damaged, and reports *selftest mm-stress: passed*.

*fpu*
	Runs three threads on every CPU that fill the x87, SSE and AVX
	registers with values of their own and check them after each of a
	thousand yields, so every switch must save and restore them.
	Reports *selftest fpu: passed* with the number of switches.

*sched-fair*
	On the last CPU, runs four fair threads of equal weight for 600 ms
	and checks that each got a quarter, within 20 %; then threads at
	nice 0 and nice 5 (reniced while running), which must split the CPU
	in proportion to their weights, 1024 to 335; then threads in two
	resource domains of CPU weights 200 and 100 next to a thread of the
	*system* domain, which must get 2:1:1. It then checks that every run
	queue's books balance.

*sched-rt*
	Checks that a real-time thread woken by one of lower priority runs
	at once, and one of lower priority waits, in 200 rounds; that a
	FIFO thread sleeping on a timer on a CPU busy with a fair thread
	wakes less than 1 ms late (median) and less than 3 ms late (90th
	percentile) over 200 sleeps; that FIFO threads of one priority run
	one after the other; and that round-robin threads of one priority
	each get at least 30 % of a CPU they share with two others.

*sched-pi*
	A FIFO thread of priority 10 holds a mutex that one of priority 30
	wants, while one of priority 20 spins on the same CPU. With priority
	inheritance the holder runs at 30, lets go, and the waiter gets the
	mutex within 150 ms and before the spinner finishes. It runs again
	with a chain of two mutexes, the holder itself waiting for a third
	thread's mutex. Needs two CPUs.

*sched-balance*
	Starts two threads per CPU, all allowed only on the last CPU, then
	allows them every CPU and checks after a second that every CPU runs
	some, no CPU runs more than two more than another, and, on a machine
	of more than one NUMA node, some moved to another node.

*lockdep*
	Seeds three bugs, each in threads of its own: two spin locks taken
	in opposite orders, two mutexes taken in opposite orders, and a
	mutex taken under a spin lock. The lock checker must report each
	(see *LOCK CHECKER*); the test asks it to keep going after the first.

*rcu*
	Readers on every other CPU follow a pointer under RCU while the
	test replaces its object and frees the old one after *synchronize*,
	100 times; a reader that ever sees a freed object fails the test.
	Reports the number of read sections and the average grace period.

*oom*
	Gives a resource domain a limit of 64 pages and checks that its
	threads' stacks fill it and the next thread is refused with an
	error. Then a thread in an app domain (made through a capability,
	as programs will get theirs) allocates until memory runs out: the
	policy must stop that app and not the service domain next to it.
	Then the service allocates while another app idles: the idle app is
	stopped first, then the service, which its supervisor restarts. No
	*system* thread may be asked to stop, and the memory must come
	back. See *radix-scheduling*(7).

*user*
	Starts programs from the *usertests* boot module, the test image of
	user programs that every image carries, and checks how each ends.
	The probe calls every system call and capability method with good
	and bad arguments and checks each answer (see *radix-ipc*(7)), and
	must exit with 0, its count of failed checks. Then eight programs
	each make one fault (a null pointer, a write to a read-only page,
	code on the stack, a privileged instruction, a jump into the
	kernel's half, an undefined instruction, a breakpoint, a division by
	zero); each must be ended by the kernel, which logs *User: thread*
	_id_ *(fault) ended:* and the fault, while the kernel and the other
	programs carry on. Last, the capability store and the object
	registry must hold what they held before the first program started.
	Skipped with a warning when the boot loader loaded no *usertests*
	module.

*ipc*
	Runs five pairs of user programs, first both on one CPU, then on
	two: a call and its reply over an endpoint, capabilities copied and
	moved in messages, notifications each side waits on, a channel of
	messages in shared memory, and messages posted to each other's ring.
	Each pair ping-pongs a thousand times and checks every answer; each
	program must exit with 0, and afterwards the capability store and
	the object registry must hold what they held before. Reports
	*selftest ipc: passed*.

*fuzz*
	Runs eight groups of three fuzzers from the *usertests* image, one
	group at a time. Each makes 4,000 random system calls, capability
	methods and ring entries, with arguments at the edges of what the
	kernel accepts (slots past the end, addresses in the kernel's half
	or not mapped, lengths that wrap, rights and flags that do not
	exist), sharing two endpoints, two notifications and a memory
	object with the other two, so calls meet partners and capabilities
	travel. Each group also gets a directory, the root of a tmpfs of its
	own mounted at _/fuzz_, and calls every directory and file method on
	it and on what it opens, with paths drawn from a few names, *.*,
	*..*, absolute paths, names and paths at and past their limits, and
	paths that hold a NUL or are not UTF-8. It mounts tmpfs, proc, sys
	and bootfs there, but never dev, whose block devices are the
	machine's disks. After the group, the kernel unmounts the tmpfs with
	everything mounted in it. A fourth program in each group, the janitor, keeps answering
	and signalling so that the fuzzers cannot all end up waiting for
	each other; whatever still waits after 30 seconds is stopped, and
	the log says in which call. After every call a fuzzer checks the
	promise of *radix-syscalls*(2): a failed call returns an error that
	the call's documentation lists, a kernel error or, for files and
	directories, one of the file domain (*radix-errors*(2)), and 0 in
	every result register. It logs each broken promise as *fuzz: broken promise* or
	*fuzz: undocumented error*, and exits with their count. At least
	half the fuzzers must finish, the capability store must pass its
	audit, and the store and the object registry must hold what they
	held before. The log names the seeds (*selftest fuzz: seeds* _a_
	*to* _b_); the *fuzz-seed* flag (*dukovo-boot*(7)) repeats them.

*bench*
	Runs the benchmark programs of the *usertests* image: the cost of
	the cheapest system calls, a fresh page and a memory copy, then round
	trips of four pairs from *ipc* (all but capabilities), 100,000 each,
	on one CPU and on two, a context switch between two programs, a TLB
	shootdown, three system calls on both CPUs at once, and opening,
	*stat* and 4 KiB reads and writes of a file in a tmpfs mounted at
	_/bench_ for the run. Each
	measurement is a line the program logs, *BENCH name=*_name_ *ns=*_ns_
	*iterations=*_count_, and after each benchmark the kernel logs what
	it did meanwhile, *BENCHCOUNT group=*_name_ *rounds=*_count_ followed
	by its switches, timer writes, interrupts sent, halts and polls,
	the nanoseconds TLB shootdowns waited for other CPUs and the CPUs
	they left to the hypervisor to flush; *ob bench* collects both.

*stack-overflow*
	Recurses until the boot CPU's stack runs into its guard page. The
	panic report must say that the kernel stack overflowed.

*smep*
	Maps a page of code in the lower, user half and calls it. With SMEP
	the call faults and the panic report says that SMEP stopped it; on a
	CPU without SMEP the test is skipped with a warning.

*smap*
	Maps a page of data in the lower half and reads it. With SMAP the
	read faults and the panic report says that SMAP stopped it; on a CPU
	without SMAP the test is skipped with a warning.

*tlb*
	Changes a kernel mapping while the other CPUs have it cached and
	checks that each sees the change after the TLB shootdown; then runs
	two address spaces, which map the same lower-half address to
	different frames, on several CPUs at once, and changes one while it
	is not running. Every CPU must always see the right frame, with and
	without PCIDs. Reports *selftest tlb: passed*.

*msr*
	Reads EFER, which every x86-64 processor has and which must say long
	mode is active, then MSR 2FFF_FFFFh, which no processor has: its #GP
	must only make the read fail, as for the temperature sensors' reads
	of registers a processor may lack. Reports *selftest msr: passed*.
	QEMU without KVM reads MSRs it does not know as 0, and there the
	test is skipped with a warning that says what it read; on aarch64,
	which has no MSRs, it is skipped too.

*caps*
	Runs the capability store's test suite in spaces of its own (see
	*radix-capabilities*(7)). It tries to use or copy capabilities it
	does not hold: empty slots, slots past the end, made-up and stale
	space names, deleted and moved capabilities, objects of another
	kind. It derives a capability with each of the 4,096 rights masks and
	checks that the result, and anything copied from it, has exactly the
	rights of the original and the mask, and that every use needing
	another right, and every handover to another space without
	*transfer*, is refused. It checks that badges are set once, that a
	sealed capability is useless without its key, that ranges only
	shrink, and that frames, DMA buffers and memory retyped from a memory
	object stay inside it, never overlap, and go back to the frame
	allocator. It revokes a tree spread over three spaces, with
	capabilities deleted from its middle, in steps, and checks that
	nothing can be derived below it in between; checks the depth limit;
	runs 100,000 random operations with the store's full audit every
	1,024; and has every other CPU derive from a tree of 4,096
	capabilities while the boot CPU revokes it, after which nothing they
	derived may be left. It finally checks that no capability, object,
	heap object or page of memory was leaked, and reports *selftest caps:
	passed*.

*iommu*
	Needs QEMU's VT-d unit or AMD IOMMU and its *edu* test device, which
	the *iommu* and *iommu-amd* cases of *ob test* add (see *IOMMU* in
	*dukovo-boot*(7)). Has the *edu* device copy by DMA from a page of
	DMA memory allocated for it, which must arrive; into a page never
	given to it, which must stay zero, with the stopped write logged
	where the IOMMU reports it (QEMU's AMD IOMMU does not); into the same
	page once mapped for it with *map_dma*, which must arrive; and again
	once taken back with *unmap_dma*, which must stay zero. Then, with
	interrupt remapping (ADR 0166), has *edu* raise its MSI with the
	message of an interrupt allocated for it, which must arrive within a
	second, and with that of one allocated for the host bridge of its
	segment, which must not arrive within 50 ms. Reports *selftest
	iommu: passed*.

*net*
	Needs a virtio network card on QEMU's user network, which the *net*
	case of *ob test* adds behind QEMU's VT-d unit (see *NETWORK* in
	*dukovo-boot*(7)). Gives the first card QEMU's address,
	10.0.2.15/24 through the router 10.0.2.2, unless the *ip* and
	*gateway* flags gave it one, then sends the router four echo
	requests, one at a time, each of which must be answered within 2
	seconds, from a socket lent its memory for the test. Logs each round
	trip, closes the socket, and reports *selftest net: passed* with the
	fastest once the socket's memory has come back to the heap (ADR
	0182).

*gpio*
	Needs QEMU's _pc-testdev_ and the SSDT of the *gpio* case of *ob
	test*, whose AMD and Intel GPIO controllers have their registers in
	the test device's memory (ADR 0233). Runs each controller's AML
	method _FIRE_, which plays the hardware: it sets a pin's interrupt
	status and pulses the controller's interrupt. The firmware's events
	on the AMD controller's pins 9 and 42 and the Intel one's pad 53
	must each run their method (_\_E09_, _\_EVT_ and _\_L35_) once
	within a second, and the level-triggered pins be unmasked after.
	Reports *selftest gpio: passed*.

*i2c*
	Needs QEMU's _pc-testdev_ and the SSDT of the *i2c* case of *ob
	test*, whose Intel LPSS and AMD DesignWare I2C controllers have
	their registers in the test device's memory (ADR 0234). Reads back
	through the SSDT what the kernel set: the Intel controller in fast
	mode with the firmware's _FMCN_ counts and SDA hold, its _\_PS0_
	run, LPSS out of reset with its DMA engine told the window's
	address; the AMD one in standard mode with counts for its 150 MHz
	clock. The devices found must be the SSDT's two. A write on _i2c-1_,
	whose memory says a stop was seen, must reach the controller and
	end; a read on _i2c-2_, whose memory says no device answered, must
	fail so; a bus that does not exist must be refused. Reports
	*selftest i2c: passed*.

# TRACE POINTS

A trace point is a named place in the kernel code that records an event
when tracing is on: the time, the CPU, the trace point and up to three
numbers. Events go into a ring of 1024 entries without taking a lock, so
trace points work in interrupt handlers and from the first Rust function,
before anything is set up. When tracing is off a trace point costs one
memory read.

Boot with the *trace* flag to record from the start of *radix_arch_entry*,
or type *trace on* in *kdb*; *trace* in *kdb* prints the ring, oldest
first. Times before timer calibration show as 0. Phase 2 has these trace
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

*ob run --gdb* adds the *nokaslr* flag, so the kernel runs at the
addresses it was linked at and GDB's symbols match. The script loads the
kernel's symbols from _out/sysroot/boot/radix_, connects to QEMU, sets a hardware breakpoint on *radix_arch_entry*, the
first Rust function, and continues. A hardware breakpoint is used because
the firmware and GRUB overwrite memory, which removes software
breakpoints set before the kernel is loaded. From there the usual GDB
commands work: *bt*, *info registers*, *break* _function_, *next*,
*print*. Add *--debug* to *ob run* for an unoptimised kernel that steps
line by line.

GDB must support x86-64; on Debian and Ubuntu the *gdb* package does.
*ob doctor* reports whether it is installed.

# SYMBOL TABLE

The kernel image reserves a 2 MiB section, *.ksyms*, for its own symbol
table, so a panic can name functions without reading debug information.
After linking, *ob build* reads the function symbols from the ELF symbol
table, demangles them, and writes them into the section in place; nothing
else in the image moves. A kernel linked without *ob* has an empty table,
and its backtraces show bare addresses. The table holds symbols in the
kernel's higher-half text only, at their link addresses; with KASLR the
kernel subtracts its slide from an address before looking it up, and adds
it to the address of a symbol it found by name.

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

*dukovo-boot*(7), *radix-capabilities*(7), *radix-scheduling*(7), *radix-ipc*(7),
*watchdog*(8), *ob*(1), *gdb*(1), *qemu-system-x86_64*(1)
