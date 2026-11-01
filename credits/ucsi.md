<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Specifications and papers
- Intel's *USB Type-C Connector System Software Interface
  Specification*, revisions 1.2 to 3.0, whose commands `radix-ucsi`
  sends to a laptop's USB-C firmware, and the USB Implementers Forum's
  *USB Power Delivery Specification*, revision 3.1, from which it reads
  power data objects and request data objects. The layout of
  `/kernel/class/typec` and the names of the ports' power supplies
  follow Linux's `ucsi` and `ucsi_acpi` (GPL-2.0)
  ([ADR 0248](docs/adr/0248-usb-c-ports-are-read-through-the-firmware-s-ucsi-and-never-set.md)).
  No code is taken from them.
