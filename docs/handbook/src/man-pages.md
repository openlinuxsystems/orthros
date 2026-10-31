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
| `radix-proc(5)` | `/processes`: the processes, the mounts, memory and uptime as text |
| `radix-sysfs(5)` | `/kernel`: the kernel's view of the hardware and its settings, one value per file, in Linux's layout |
| `dukovo-boot(7)` | How Dukovo boots, its consoles, the kernel command line and microcode |
| `radix-debug(7)` | Panic reports, the crash log, the kernel debugger, the lock checker, GDB and trace points |
| `radix-capabilities(7)` | Capabilities: object kinds, rights, badges, sealing, delegation and revocation |
| `radix-scheduling(7)` | Threads, scheduling classes, priority inheritance, RCU, resource domains and the out-of-memory policy |
| `radix-ipc(7)` | User mode, the system calls, endpoints, notifications, channels, rings and the random capability |
| `radix-processes(7)` | Processes, program loading, the address space layout and the boot archive |
| `radix-files(7)` | Files, directory capabilities, paths, filesystems, the page cache, the directory hierarchy and `/volumes` |
| `radix-posix(7)` | POSIX programs: the C library and toolchain, fork and exec, signals, descriptors and paths, pipes, terminals, timers, threads, limits, file locks, dynamic linking and Rust's standard library |
| `spark(8)` | Spark, the first program, which starts and supervises every other |
| `service(8)`, `shutdown(8)`, `reboot(8)` | Starting, stopping and looking at services; stopping, restarting or powering off the machine |
| `consoled(8)` | The virtual consoles on the screen and keyboard, and their keys |
| `authd(8)`, `user(8)`, `passwd(1)`, `login(1)` | Accounts: the service that keeps them, checks passwords and starts sessions, making and listing accounts, changing a password, and signing in at a console |
| `elevate(8)`, `sudo(8)` | Running a command as an administrator, after the password once in each terminal pane every 15 minutes |
| `nukesh(1)` | Nukesh, the shell: options, start-up files, scripts, the line editor, history, completion, job control, POSIX mode and its limits |
| `coreutils(1)`, `7zz(1)` | What the ported core utilities and 7-Zip do on Dukovo |
| `systools(1)`, `ps(1)`, `top(1)`, `free(1)`, `df(1)`, `uptime(1)`, `w(1)`, `who(1)`, `pidstat(1)`, `vmstat(8)`, `lsof(8)`, `scrub(8)`, `subvolume(8)`, `mkswap(8)`, `swapon(8)`, `swapoff(8)`, `dmesg(1)`, `kill(1)`, `pgrep(1)`, `renice(1)`, `ionice(1)`, `taskset(1)`, `watch(1)`, `tree(1)`, `hexdump(1)`, `xxd(1)`, `column(1)`, `which(1)`, `script(1)`, `chmod(1)`, `chown(1)` | Dukovo's own administration tools, with the Linux tools' names, options and output |
| `logd(8)`, `log(1)`, `journalctl(1)`, `logtrim(8)`, `log(5)` | The system's log and its audit log: what logd keeps, how to read it by service, level, time and boot, and how much it keeps |
| `notifyd(8)`, `notify-send(1)`, `notifications(1)` | Notifications: sending them, how the console shows them, and listing them |
| `dukovo-privacy(7)` | The privacy promise: what Dukovo keeps on the machine and what it sends on its own |
| `rtrace(1)` | Tracing a process's system calls and capability messages |
| `radix_cap_call(2)`, `radix_send(2)`, `radix_recv(2)`, `radix_reply_recv(2)`, `radix_notify(2)`, `radix_wait(2)`, `radix_yield(2)`, `radix_map(2)`, `radix_unmap(2)`, `radix_ring_setup(2)`, `radix_ring_enter(2)`, `radix_debug_log(2)` | The twelve system calls, one page each, with their registers and errors |
| `radix-capability(2)`, `radix-memory(2)`, `radix-address-space(2)`, `radix-thread(2)`, `radix-endpoint(2)`, `radix-notification(2)`, `radix-random(2)`, `radix-ring(2)`, `radix-kernel-log(2)`, `radix-process(2)`, `radix-signal(2)`, `radix-terminal(2)`, `radix-directory(2)`, `radix-file(2)`, `radix-resource-domain(2)`, `radix-framebuffer(2)`, `radix-input(2)`, `radix-power(2)`, `radix-network(2)`, `radix-names(2)`, `radix-trace(2)`, `radix-startup(2)`, `radix-errors(2)` | The interfaces of each kind of object, the start-up page and the kernel's errors |

The software Dukovo ports from elsewhere brings its own pages, such as
`grep(1)`, `sed(1)`, `tmux(1)`, `nano(1)` and `bsdtar(1)`, and `man -k`
searches them all with Dukovo's own.

Sections follow the usual convention: 1 user commands, 2 system calls,
3 library interfaces, 4 devices and drivers, 5 file formats, 7 overviews
and concepts, 8 administration.
