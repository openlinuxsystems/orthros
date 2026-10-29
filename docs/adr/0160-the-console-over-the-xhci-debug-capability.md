<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0160. The console goes over the xHCI Debug Capability to a second computer, with the `dbc` flag

- Status: Accepted
- Date: 2026-10-28

## Context

The kernel's log, its panic report and `kdb` (radix-debug(7)) reach a
developer through the serial port, and through the screen. The reference
machines have no serial port: the Dell Inspiron 15 5570 has none, nor
has the Framework Desktop, nor most PCs since about 2015. On them a
panic is a photo of the screen, and `kdb` cannot run.

Every xHCI controller may have a *Debug Capability* (xHCI 1.2, 7.6), and
Intel's and AMD's of the last decade do: once software enables it, the
first of the controller's USB 3 root ports that a *debug host* is plugged
into stops being a root port, and the controller is a USB device on it,
with one bulk endpoint each way. The debug host is a second computer,
joined with a USB 3 A-to-A debug cable (its data lines crossed and its
power line cut, as the specification asks). The capability has its own
registers, event ring and transfer rings, works while the rest of the
controller is busy or stopped, and raises no interrupt: it is polled.

Linux uses it in two ways: `earlyprintk=xdbc`, a console from the first
moments of boot that it drives on its own, and `xhci-dbgcap`, which makes
it a TTY once the xHCI driver runs. Both give it the IDs 1d6b:0010 (the
Linux Foundation's vendor ID, and "Linux USB Debug Target"), and a Linux
debug host's `usb_debug` driver takes a device with those IDs as a
serial port, `/dev/ttyUSB0`. Windows' kernel debugger uses the same
capability with its own protocol. QEMU's xHCI controller has no Debug
Capability.

## Decision

- With the new `dbc` flag, the first xHCI controller whose capabilities
  list a Debug Capability turns it on, from its thread, once the
  controller runs; the console is then on it. Without the flag, it stays
  off. The other controllers say they leave theirs off, and one with none
  says so.
- `radix-xhci` drives it (`dbc::Dbc`), as Linux's `xhci-dbgcap` does:
  - it is turned off, given its event ring, its two transfer rings and
    its context (the strings and IDs a debug host enumerates it from),
    and turned on, then waits for a debug host to connect and to
    configure it;
  - a debug host that has not configured it within 5 s has it turned off
    and on again, which makes it try again;
  - one write is in flight at a time, a page at most, and one read of a
    packet waits for the debug host's bytes; a stalled transfer is made a
    No Op if its endpoint still points at it, so it does not go again
    once the debug host clears the halt, and a transfer that waited
    during a halt is rung again after;
  - a cable pulled out, or a port the debug host resets, ends what was in
    flight and empties the rings for the next; a capability found off (a
    reset of the controller turns it off) is set up again;
  - transfer events are matched by their TRB's address, since AMD's and
    Intel's controllers number the endpoints differently.

  Nothing in it waits, so the kernel can drive it from anywhere. It is
  tested against a simulated capability and debug host.
- It is told it is Linux's 1d6b:0010, revision 0010, with the GNU Remote
  Debug protocol number Linux gives it, so that any Linux machine reads
  it as a serial port with no setup; its strings say Dukovo and "Radix
  debug console".
- The kernel log reaches it through a sink that copies into a ring of the
  last 64 KiB, replaying the log so far when it is turned on; the
  controller's thread polls the capability every 10 ms, which starts a
  write of what waits and keeps what the debug host typed. `kdb` (on the
  serial port and on the capability alike, through a new
  `debug_console`) and the panic path poll it themselves, since the
  thread may not run then; after a panic the report has up to 2 s to go
  out before `kdb` or the restart. At the end of boot `kdb` waits for the
  controllers to look for a capability before deciding there is no debug
  console.
- Its lock is taken before the rings' locks, and nothing is logged while
  either is held; the log's sink takes only the output ring's.

## Consequences

- A machine with no serial port can be debugged from a second computer
  with a debug cable: the log, a panic report and `kdb`, over
  `/dev/ttyUSB0` on a Linux debug host. radix-debug(7) says how.
- QEMU has no Debug Capability, so the boot tests only see the flag
  looked for and declined (`usb-devices`); the driver is tested against
  its simulation, and is first tried on real hardware with the reference
  machines' controllers (Intel on the Dell, AMD on the Framework).
- The console reaches the capability only once its controller runs, a
  few hundred milliseconds into boot; what was logged before is replayed
  then, but a panic before that is not seen there. Linux's
  `earlyprintk=xdbc`, which runs the capability before anything else, is
  left for when it is needed.
- Linux's early console resets a few of an Intel controller's USB 3
  ports, ones nothing is connected to, so that the debug host's link
  comes up; its run-time driver does not, and neither does Radix until a
  machine needs it.
- While the capability carries the console, its controller's thread
  wakes every 10 ms, which costs power: the flag is for debugging.
