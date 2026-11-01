<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0246. USB serial ports are terminals whose master is the kernel, named as Linux names them

- Status: Accepted
- Date: 2026-11-01

## Context

Serial ports now come on USB: modems, boards such as the Arduino and
the Raspberry Pi Pico, consoles of routers and servers through an
adapter, and GPS receivers. Most speak the Abstract Control Model of
the *USB Communications Class Subclass Specification for PSTN Devices*
(revision 1.2), and Linux's `cdc_acm` names them `ttyACM0` and on. Most
adapters that do not are FTDI's chips, with vendor requests of their
own (FTDI's application notes AN232B-04 and AN232B-05), which Linux's
`ftdi_sio` names `ttyUSB0` and on. QEMU's `usb-serial` is an FTDI
FT232BM.

Programs that use a serial port (screen, minicom, picocom, stty, the
Arduino tools, pyserial) open it as a terminal and set its line with
termios: the speed, character size, parity, stop bits and flow control.
The kernel's terminals (`fs/tty.rs`) already have a line discipline,
settings and job control, shared by the pseudo-terminals and the
consoles.

Linux's `cdc-acm.c` and `ftdi_sio.c` are GPL and were not used. The
divisor encoding is FTDI's, from AN232B-05 and the FT232H and FT2232H
data sheets.

## Decision

- `radix_usb::serial`, in a compartment with no unsafe code, finds a
  configuration's serial ports (a CDC-ACM control interface and the data
  interface its union names, or each interface of an FTDI chip), reads a
  termios `c_cflag` and `c_ospeed` into a line's settings, and builds
  each protocol's requests: SET_LINE_CODING and SET_CONTROL_LINE_STATE
  for CDC-ACM, and FTDI's reset, modem control, flow control, data and
  baud rate requests, with the divisor for each chip's clock.
- Each port is a terminal whose master side is the kernel's driver
  (`tty::make_line`): `/devices/ttyACM0` or `/devices/ttyUSB0` and on,
  numbered as they come and given back when unplugged. What comes in on
  the bulk IN endpoint is typed on the terminal, held while the terminal
  cannot take it; what programs write goes out on the bulk OUT
  endpoint. A change of settings, an open or a close wakes the USB
  controller's thread, which sets the line as Linux does: DTR and RTS
  up while a program has it open, down after the last close if HUPCL is
  set, and at B0.
- The ports run on xHCI controllers, whose thread carries them as it
  carries keyboards. The polled EHCI, OHCI and UHCI controllers (ADR
  0243) carry no bulk transfers yet, and the log says their serial
  ports are not carried.
- Unplugging a device hangs its terminals up: readers see end of file,
  and the session's processes get SIGHUP.

## Consequences

- The boot test `usb-serial` plugs two of QEMU's FTDI ports into one
  socket, so that what is written to one comes in on the other: it sets
  their speed with stty, sends text each way, checks lsusb's driver
  names, and unplugs one under a reader.
- CDC-ACM is tested against descriptors and requests in host tests;
  QEMU has no CDC-ACM device.
- Other adapters with protocols of their own (Prolific PL2303, Silicon
  Labs CP210x, WCH CH340) are not driven yet; they are left for a later
  driver of the same shape.
