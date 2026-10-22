<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Manual pages

Every Dukovo command, file format and interface has a manual page.
`./ob docs` compiles them into `out/docs/man/`; read one with
`man -l out/docs/man/man1/ob.1`. The sources are in `docs/man/`, except
for section 2, which `ridlc` generates from the interface definitions in
`abi/*.ridl` so that the pages always match the code.

| Page | Describes |
| --- | --- |
| `ob(1)` | The build tool: every command and option |
| `dukovo.toml(5)` | The build configuration file |
| `radix-proc(5)` | `/proc`: the processes, the mounts, memory and uptime as text |
| `radix-sysfs(5)` | `/sys`: the kernel's devices and settings, one value per file, in Linux's layout |
| `dukovo-boot(7)` | How Dukovo boots, its consoles, the kernel command line and microcode |
| `radix-debug(7)` | Panic reports, the crash log, the kernel debugger, the lock checker, GDB and trace points |
| `radix-capabilities(7)` | Capabilities: object kinds, rights, badges, sealing, delegation and revocation |
| `radix-scheduling(7)` | Threads, scheduling classes, priority inheritance, RCU, resource domains and the out-of-memory policy |
| `radix-ipc(7)` | User mode, the system calls, endpoints, notifications, channels, rings and the random capability |
| `radix-processes(7)` | Processes, program loading, the address space layout and the boot archive |
| `radix-files(7)` | Files, directory capabilities, paths, filesystems, the page cache, the directory hierarchy and `/volumes` |
| `spark(8)` | Spark, the first program, which starts every other |
| `radix_cap_call(2)`, `radix_send(2)`, `radix_recv(2)`, `radix_reply_recv(2)`, `radix_notify(2)`, `radix_wait(2)`, `radix_yield(2)`, `radix_map(2)`, `radix_unmap(2)`, `radix_ring_setup(2)`, `radix_ring_enter(2)`, `radix_debug_log(2)` | The twelve system calls, one page each, with their registers and errors |
| `radix-capability(2)`, `radix-memory(2)`, `radix-address-space(2)`, `radix-thread(2)`, `radix-endpoint(2)`, `radix-notification(2)`, `radix-random(2)`, `radix-ring(2)`, `radix-kernel-log(2)`, `radix-process(2)`, `radix-directory(2)`, `radix-file(2)`, `radix-startup(2)`, `radix-errors(2)` | The interfaces of each kind of object, the start-up page and the kernel's errors |

Sections follow the usual convention: 1 user commands, 2 system calls,
3 library interfaces, 4 devices and drivers, 5 file formats, 7 overviews
and concepts, 8 administration.
