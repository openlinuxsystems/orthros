<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0096. The native administration tools are one Rust program, written for Radix with Linux's names and options

- Status: Accepted
- Date: 2026-10-23

## Context

The roadmap's administration command set (Phase 9 rows, and the
Radix-native tools of the architecture table) asks for the commands an
administrator types on Ubuntu Server: ps, top, kill, pgrep, pkill, who,
w, users, uptime, free, vmstat, df, which, chmod, chown and chgrp;
renice, ionice, taskset, lsof, an strace, dmesg, pidstat, watch and
script; xxd, hexdump, column and tree.

Linux's own tools come from procps-ng, util-linux, sysstat, strace,
lsof, GNU coreutils, Vim and Steve Baker's tree. Most of them read
Linux's `/proc` files by their formats, utmp, `ptrace` or Linux system
calls that Radix does not have and will not imitate (ADR 0094,
ADR 0095), so porting them would mean emulating Linux under them. Most
are also under the GPL, and ADR 0087 keeps Dukovo's own programs
permissive: a GPL tool could only be a port of its own, and these would
need Radix-specific changes throughout. The rest (xxd, hexdump, column,
tree) do not depend on Linux, but they are small, and a native version
shares the other tools' argument parsing and output code.

## Decision

The tools are written for Dukovo in Rust, as one program,
`/system/bin/systools`, in `userspace/programs/systools`. Each tool's
name is a link to it, and it runs the tool its name says, as uutils'
coreutils does (`systools ps -ef` works too). It depends only on the
`libc` crate and Radix's generated system call interface (`radix-abi`),
for what the C library does not wrap: affinity, I/O classes and traces.

Each tool takes the name, options and output of the Linux tool
administrators know: procps-ng's for ps, top, kill, pgrep, pkill, free,
vmstat, w, uptime and watch; util-linux's for renice, ionice, taskset,
dmesg, script, column and hexdump; GNU coreutils' for chmod, chown,
chgrp, df, who and users; sysstat's for pidstat; lsof's for lsof;
strace's for rtrace; Vim's for xxd; and tree 2.1's for tree. Their
outputs were compared with those tools' on Linux as they were written.
No code is taken from any of them: they are written from their manual
pages and their observed behaviour. Where Radix lacks something, a tool
says so in its manual page and behaves as Linux would for that case:
until users come every process is root's, chmod and chown change
nothing (radix-posix(7)), and `lsof -i` finds no network files.

They read the kernel through `/proc` (radix-proc(5)), `/dev/kmsg`, and
radix-process(2)'s methods. lsof also lists every capability a process
holds, not only its files, since that is what a Radix process holds.
rtrace prints a trace's records (ADR 0095) as strace prints its lines,
with each capability method by name.

Every tool has a manual page in `docs/man`, and the program's unit tests
run on the build host.

## Consequences

- An administrator's habits carry over: `ps aux`, `top -b -n1`,
  `free -h`, `lsof -p`, `rtrace -f`, `dmesg -w` and `watch -n1` do
  what they do on Linux.
- One program of about 1.6 MiB serves 30 names, and its code for
  arguments, tables, `/proc` and time is shared.
- The tools are ours to keep in step with Radix: a new field in
  radix-proc(5) can appear in ps the same day.
- Options that change only colours or pagers are taken and ignored, so
  scripts that pass them run; options for Linux features Radix lacks are
  refused, or say so, rather than pretending.
- Their behaviour is checked against Linux's tools by hand as they are
  written and by the posix boot test's scripts afterwards; a change in
  a Linux tool's output does not reach Dukovo by itself.
