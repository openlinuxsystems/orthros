<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Manual pages

Every OrthrOS command, file format and interface has a manual page.
`./ob docs` compiles them into `out/docs/man/`; read one with
`man -l out/docs/man/man1/ob.1`. The sources are in `docs/man/`.

| Page | Describes |
| --- | --- |
| `ob(1)` | The build tool: every command and option |
| `orthros.toml(5)` | The build configuration file |
| `orthros-boot(7)` | How OrthrOS boots, its consoles, the kernel command line and microcode |
| `radix-debug(7)` | Panic reports, the crash log, the kernel debugger, GDB and trace points |
| `radix-capabilities(7)` | Capabilities: object kinds, rights, badges, sealing, delegation and revocation |

Sections follow the usual convention: 1 user commands, 2 system calls,
3 library interfaces, 4 devices and drivers, 5 file formats, 7 overviews
and concepts, 8 administration.
