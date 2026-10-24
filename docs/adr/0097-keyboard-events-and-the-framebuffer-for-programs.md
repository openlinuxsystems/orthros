<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0097. The kernel hands programs key events through /dev/kbd and the screen through /dev/fb0

- Status: Accepted
- Date: 2026-10-23

## Context

Phase 9's exit is a machine that boots to a Nukesh prompt, in QEMU and
on the reference Dell, with virtual consoles an administrator switches
with Alt+F1 to Alt+F6. Until now the screen was only the kernel log's
(ADR 0015), and the only keyboard input came through the serial port or
the boot-time poll of kdb.

Two designs were possible. A Linux-style console in the kernel would
keep the terminal emulator, the key maps and the virtual terminals in
kernel code, where a bug in an escape-sequence parser is a kernel bug.
Radix's rule is the opposite: what does not need the kernel's privilege
runs as a program. The kernel needs only to give a program the keys and
the pixels.

QEMU's default keyboard and the Dell's built-in one are PS/2 keyboards
behind an i8042 controller. USB keyboards come with the USB stack in
Phase 11; until then firmware usually emulates them as PS/2 ones.

## Decision

- The x86_64 layer drives the i8042 controller: it sets it up as the
  OSDev wiki's "8042 PS/2 Controller" page describes, short of the
  self-tests that reset some controllers, keeps scan code set 1
  translation on, and routes ISA interrupt 1 through the I/O APIC.
- A new library, `radix-input`, decodes set 1 into Linux's key codes
  (<linux/input-event-codes.h>), so key maps and programs written for
  evdev read them as they would on Linux. It is host-tested.
- `/dev/kbd` (radix-input(2)) gives the events as fixed 16-byte records:
  a time, a key code and pressed, repeated or released. The interrupt
  puts them in a ring of 512; each open description keeps its own place,
  starting after it was opened, so no program reads keys typed before
  it. The interrupt wakes a `kbd` thread, which wakes readers and
  `poll`, because waking takes a lock interrupt handlers must not take.
  The same thread lights the lock lights on request (SET_LEDS).
- `/dev/fb0` (radix-framebuffer(2)) is the framebuffer the firmware set
  up. GET_INFO describes it; TAKE makes a description its only writer
  and keeps the log off the screen, though the kernel keeps the log's
  grid up to date; GIVE_BACK, or the description's end, draws the log
  again. Writes are copied through a kernel buffer in 16 KiB steps,
  since the screen's lock masks interrupts and copying from a program
  may wait for a page. A panic report draws over whatever a program
  drew.
- The kernel turns no key into text. Key maps, the terminal emulator and
  the virtual terminals live in consoled (ADR 0100).

## Consequences

- A crash in the terminal emulator ends a program, which Spark starts
  again, and gives the screen back to the log.
- Anything that holds `/dev/kbd` sees every key, so it must stay with
  the processes that need it; until users come (Phase 13) every process
  is root's and could open it.
- Programs ported from Linux that read evdev codes need only their
  device path changed.
- Mice, USB keyboards and other layouts are left for later: the second
  i8042 port's bytes go nowhere until a mouse driver wants them, and USB
  keyboards arrive with Phase 11's USB stack.
