<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Specifications and papers
- RISC-V International's *RISC-V Supervisor Binary Interface
  Specification*, version 2.0, whose base, Debug Console, Timer and
  System Reset extensions the RISC-V smoke boot calls, *The RISC-V
  Instruction Set Manual, Volume II: Privileged Architecture*
  (20211203), from which it turns Sv39 paging on and takes a timer
  interrupt, and the Devicetree Specification (0.4), from which it reads
  the machine's model and timebase
  ([ADR 0270](docs/adr/0270-a-risc-v-smoke-boot-keeps-the-kernel-s-boot-path-portable.md)).
