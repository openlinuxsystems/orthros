<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0214. Names on processes and capabilities, and the second gate

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.2. Files have owners, groups and modes since 13b.1 (ADR
0213), and nothing checks them. The security design (sections 3 and 5)
says how they come into force:

- Every process acts for a **name**: a user (alice, from 1000), a
  service account, or the admin label 0, shown as root, inside elevate.
  POSIX sees it through `getuid` and its kin. A name never grants
  anything.
- **Two gates on every file.** The capability decides what may be
  reached and the most that may be done there; the file's owner, group
  and mode then narrow that for whoever asks, as on Linux. Ownership
  only ever takes away.
- An **override** right on a directory capability skips the second gate
  inside that subtree only: Linux's machine-wide CAP_DAC_OVERRIDE, made
  as small as the capability. The ABI names it **bypass**, since
  `override` is a word Rust and C++ keep for themselves.
- **The name travels with the capability.** A directory capability
  carries the name it acts for, and what is opened or made through it
  keeps it, so a service handed alice's folder acts as alice there and
  nowhere else, which closes the confused deputy. Only Spark and authd
  may give another name.
- A program debugs only programs it started, and `/processes` shows
  other users' processes by name only.

The second gate is needed because shared places hold files of many
owners (`/shared`, `/tmp`, `/system/settings`), and because programs
from Linux expect it: PostgreSQL refuses a data folder that is not its
own with mode 0700, and sftp, tar and rsync carry owners and modes.

## Decision

- **A name** is a user number, a group number and up to 32 more groups
  (POSIX's supplementary groups). Each process has one, and so does each
  file and directory object (the "stamp" of the security design), which
  is what the second gate checks and what owns the files made through
  it.
- **Where names come from.**
  - The kernel starts Spark as root (user 0, group 0), its root
    directory with every right, bypass among them, and gives it the
    **names capability** (`names`, kind 20, radix-names(2)), of which there is
    one.
  - A process made by `create` or `fork` acts for its maker's name, and
    `exec` keeps it. With the names capability, `name_process` gives a
    process that has not started another name.
  - An object opened through a directory takes that directory's name;
    one a process makes without a path (pipes, sockets, terminals) takes
    the process's. With the names capability, `stamp` puts a capability
    to the same directory, with the same rights, and another name, in a
    slot.
  - So a name only changes where a holder of the names capability
    changes it. Spark keeps it; 13b.5 gives it to authd for sign-in.
- **The bypass right** is right 12 of radix-capability(2), on
  directories and files. It is carried from a directory to what is opened
  through it, so that it covers the subtree, and dropped like any right
  by deriving a capability without it. Spark's root has it, and so for
  now do the services, which share Spark's root until their files
  declare what they get (13b.4).
- **The gate**, as Linux's without access lists (those come in 13b.7):
  the owner's bits if the name's user owns the node, else the group's if
  the node's group is the name's group or one of its others, else the
  others'. Bypass passes it, but for running a file without an execute
  bit, as for Linux's root. A call that takes two paths (`rename`, `link`,
  `snapshot`) checks each through its own capability's name.
  - **Walking a path** needs search (execute) on every directory it
    looks in, the first included.
  - **Opening** needs read for the read right and write for the write
    right or to truncate; a file the open makes is the caller's whatever
    its mode. Listing a directory needs read, checked by `list`, so that a
    directory may be opened and entered with search alone.
  - **Making, removing and renaming** need write and search on the
    directory, and on both for a rename; moving a directory to another
    needs write on it, for its `..`. In a sticky directory only the
    owner of the file or of the directory removes or renames a file.
    Removing a subvolume needs write and search on it too, as Linux's
    `user_subvol_rm_allowed` has it.
  - **A hard link** needs the file's owner, or read and write on it, as
    Linux's `fs.protected_hardlinks` has it.
  - **chown** of the owner needs bypass. A file's owner may give it a
    group of its own name, and anyone with bypass any. **chmod** needs
    the owner or bypass, and drops the set-group-ID bit when the file's
    group is not one of the name's, without bypass, as Linux does. A
    chown that changes neither, `chown -1 -1`, needs nothing, unless it
    would take a set-ID bit away.
  - **Setting times** to given values needs the owner or bypass; to
    now, the owner, write permission or bypass, as `touch` needs.
  - **Running a program** (`exec`) needs execute on the file, and on a
    script's interpreter. Unlike Linux it needs read too, since the
    program is opened with the read right to be loaded: a 0711 program
    runs only for its owner.
  - `access` and `faccessat` ask the same through a new `check_access`
    (radix-directory(2) 32), which also says EROFS for a write to a
    read-only filesystem.
  - Reading a symbolic link, `stat` and everything on an open descriptor
    but `fchmod`, `fchown` and `futimens` check nothing more, as on
    Linux, where the open was checked. A directory's read is checked
    when it is listed, so that a capability may name one it cannot list;
    the C library asks `check_access` when it opens one to read, so that
    `opendir` fails with EACCES as on Linux, and `chdir` for search.
    mlibc's `readdir` gave up the program on an error, and now returns
    it, as POSIX has it (patch 0049).
- **POSIX.** `getuid`, `geteuid`, `getgid`, `getegid` and `getgroups`
  give the process's name, through `identity` (radix-process(2) 55).
  `setuid`, `setgid`, `setgroups` and their kin succeed when they change
  nothing and fail with EPERM otherwise: there is no setuid, and a
  process gets another name only when it is started.
- **Signals and tracing.** A process may signal itself, its descendants,
  and the processes of its session that act for the same user. It may
  trace only its descendants, as Linux's Yama has it at level 1, root
  included: tracing others comes with the admin set of elevate (13b.6)
  and `processes.debug` (13d), so until then `rtrace -p` reaches only
  what a shell started for it (`cmd & exec rtrace -p $!`).
- **/processes.** Each process's directory belongs to its user and
  group with mode 0555. Its `status` (name, state, parent, times,
  memory) is 0444, and its `cmdline`, `files` and `threads` 0400. So a
  capability to `/processes` acting for alice sees bob's processes by
  number, name and state only; one with bypass sees everything. `status`
  gains `user` and `user_group` lines, which ps, top, pidstat and lsof
  show, and ps without options lists the user's own processes, as on
  Linux.

## Consequences

- Nothing changes for what runs today: Spark, its services and the
  programs they start act for root with bypass, so they pass the
  second gate as root passes Linux's, and every benchmark row keeps its
  speed; the gate costs a read of each directory's owner and mode only
  for a name without bypass.
- A file needs an execute bit to run, as on Linux, for root too.
- The new `principals` boot test, which Spark starts with `--names` so
  that it holds the names capability as descriptor 3, starts processes
  as alice and bob, each with a root stamped for them and derived
  without bypass. It checks that alice cannot read bob's 0700 file
  through a capability to `/` that reaches it, chown, remove bob's files
  from a sticky directory, run a file with no execute bit, signal or
  trace bob's processes, or see more than their status in `/processes`;
  and that root's bypass right, and a capability stamped for bob that
  keeps it, can.
- **Not yet:** users' names and the accounts behind them (`passwd` and
  `group` from authd, 13b.5), and the user of a Unix socket's peer,
  which `SO_PEERCRED` still gives as 0 (13b.5); who, w and users still
  show every session as root's until sessions come (13b.3); access lists and mounts owned by a user
  (13b.7); a write taking away set-user-ID bits, which mean nothing
  where no program runs as its file's owner; Linux's
  `fs.protected_symlinks`, which a private `/tmp` per session (13b.3)
  makes less needed.
