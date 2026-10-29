<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0148. USB keyboards and mice are read through their HID reports, and their keys repeat in software

- Status: Accepted
- Date: 2026-10-28

## Context

ADRs 0146 and 0147 brought the xHCI driver and hubs: every USB device is
found, addressed and configured, but no driver reads one. The consoles
(ADR 0100) take their keys from `/devices/kbd` (ADR 0097), which only the
PS/2 keyboard fills. Most machines have no PS/2 keyboard: the Framework
Desktop has none, and a laptop's own keyboard is the only one a PS/2
controller reaches; every keyboard and mouse plugged in is USB.

USB keyboards, mice and tablets are HID devices (*Device Class Definition
for Human Interface Devices 1.11*). A HID interface sends *Input reports*
on an interrupt endpoint and describes their layout in a *report
descriptor*: fields of values, each named by usages from the *HID Usage
Tables*. Every keyboard and mouse made for a PC also speaks the *boot
protocol*, whose reports are fixed (6 keys at a time and three buttons),
so that firmware can read them without a parser; many keyboards say more
in their own reports (every key at once, media keys), and a tablet, which
says where it points rather than how far it moved, has no boot protocol
at all. QEMU's usb-tablet is one, and the pointer of most virtual
machines.

Linux reads every HID device through its report descriptor (`hid-core`
and `hid-input`), with the boot protocol left to firmware; FreeBSD's
`ukbd` reads keyboards in the boot protocol and its `hkbd` and `hms`
through the descriptor. A PS/2 keyboard repeats a held key by itself; a
USB one does not, and Linux's input core repeats keys in software for
every keyboard (250 ms, then every 33 ms, its defaults).

## Decision

- HID interfaces are read by their controller's thread, as hubs are: once
  a device is configured, each of its HID interfaces (at most four) has
  its report descriptor read, is asked to report only changes (SET_IDLE
  0), and has a transfer started on its interrupt IN endpoint; each
  report that comes is decoded and the transfer started again. A boot
  interface whose descriptor cannot be read, or describes more than the
  decoder keeps, is put in the boot protocol and read with its fixed
  layout. An interface whose reports keep failing is given up and
  logged.
- A new crate, `radix-hid`, knows nothing of USB or of the kernel: it
  lays out a report descriptor (`Layout`: at most 64 fields, 256 usages,
  32 reports, every item checked against the bytes there are, and a
  descriptor that describes more refused rather than cut short) and
  decodes Input reports into key events and pointer motion (`Decoder`).
  It is written from the HID specification and the Usage Tables; its
  usage-to-key table names each usage of the Keyboard/Keypad page, the
  Consumer page's media and application keys and the system keys by
  radix-input(2)'s key codes, which are Linux's, so a key means the same
  as on Linux. HID over I2C, for laptops' touchpads (Phase 17), can use
  it unchanged.
- Keys go to `/devices/kbd`, next to the PS/2 keyboard's, so consoled
  needs no change: modifiers first, then the keys let go of, then the
  keys pressed. The `kbd` thread repeats the last key a USB keyboard
  pressed as Linux's input core does, 250 ms after it went down then
  every 33 ms, until a key goes up; a late repeat goes on from then, with
  no burst to catch up. A device unplugged lets go of every key and
  button it held.
- Pointers go to a new device, `/devices/mouse`: a ring of Motion records
  (radix-input(2)), read as `/devices/kbd` is, each with how far the
  pointer moved, its wheels, its buttons after the report, and, for a
  tablet, where it points, scaled to 0 to 65535 on each axis. The
  Lumen compositor (Phase 17) will read it; nothing does yet.
- The lock lights consoled asks for (SET_LEDS) are sent to every USB
  keyboard as an Output report (SET_REPORT), and to each keyboard
  plugged in later.
- radix-input(2) gains Motion, the mouse buttons, and the key codes of
  the Japanese and Korean keys, the editing keys (Undo, Copy, Paste and
  others), the media keys and the brightness keys a USB keyboard has.

## Consequences

- The boot test `usb-input` types on a USB keyboard and on the PS/2 one
  into the same console, holds a key to see it repeat, moves a USB mouse
  and clicks it, points a USB tablet, and pulls out the keyboard while a
  key is held, which must stop its repeat. `radix-hid`'s host tests
  decode the boot protocol, a tablet, and a composite keyboard with media
  keys, and check that any descriptor and any reports, however wrong,
  are safe.
- The decoder's limits fit the keyboards, mice and tablets that exist;
  a gamepad or a drawing tablet with more fields is refused at the
  interface, logged, and left for when one is needed.
- Repeat is per machine, not per keyboard, as on Linux's consoles; the
  delay and period are fixed until Settings can change them.
- Keyboards are read in US English; other layouts are consoled's
  keymaps, next.
