<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0213. Files have owners, groups and modes

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.1. The plan puts users, groups and Unix permissions in Phase
13 as a second gate after capabilities: a capability says what a
program may reach at all, and a file's owner and mode then narrow what
its user may do there (the security design, sections 5 and 6). The
second gate needs something to check, so this step comes first: files
get owners, groups and permission bits that the filesystems keep, and
the calls that make and change them. The checks come with users in
13b.2.

Until now nothing kept them. The C library made them up in `stat`
(0755 for every file and directory, owned by user 0), and `chmod`,
`chown` and `umask` checked their arguments and changed nothing. Linux
programs read and write modes all the time: `ls -l`, `tar`, `cp -p`,
`install -m`, compilers that make their output executable, `mkstemp`
asking for 0600. Btrfs and ext4 disks that Linux wrote already hold
owners and modes, which Radix ignored.

## Decision

- **The VFS.** A node has an `Access`: its owner's number, its group's,
  and its mode, the twelve permission bits (read, write and execute for
  the owner, the group and the others, with set-user-ID, set-group-ID and
  sticky). `Node::access` gives it, and `Node::set_access` changes the
  owner, group or mode and the change time. The calls that make a node
  (`create`, `make_link`, `make_node`, `make_socket`, and `open` with
  `create`) take the access it is made with.
- **What a new node gets.** The kernel gives it the mode the program
  asked for less the process's file mode mask, which the kernel already
  kept for `umask` and `exec`. It belongs to the caller's user and group,
  all user 0 until 13b.2. In a directory with the set-group-ID bit it
  takes the directory's group instead, and a new directory takes the bit
  too, as on Linux. `mkdir` drops the set-user-ID and set-group-ID bits
  from the mode it is given, as Linux does; a symbolic link's mode is
  0777; a socket's name gets 0777 less the mask.
- **chown and chmod.** A change of owner or group takes a file's
  set-user-ID bit away, and its set-group-ID bit if its group may run it,
  even when both stay, as Linux does, so that a program never keeps them
  for a new owner; a directory keeps them. Each filesystem makes the
  change under the lock that keeps the node's access, so that two
  changes cannot cross. A symbolic link's mode cannot change
  (EOPNOTSUPP, as Linux's `fchmodat` with `AT_SYMLINK_NOFOLLOW`).
- **The filesystems.**
  - tmpfs keeps them in memory. A mounted tmpfs's root is 1777, as on
    Linux; the kernel's root tree is 0755. Spark narrows the tmpfs it
    mounts in `/system` (`settings`, `state`, `live`) to 0755, and
    `/tmp` stays 1777.
  - Btrfs keeps them in the inode item (`uid`, `gid`, `mode`), and ext4
    in the inode, both halves of the 32-bit owner and group, as Linux
    does unless mounted with `no_uid32`. Disks Linux wrote show their
    owners and modes as Linux shows them.
  - FAT and exFAT keep none. Their files and directories are user 0's
    with mode 0755, as Linux's vfat and exfat show them with their
    default umask. As there, a change that keeps the owner and group
    they show and sets permission bits alone is taken and changes
    nothing, so that `cp -p`, `cp -a` and `tar` copy to a USB stick
    without an error; a chown to anyone else, or a mode with set-user-ID,
    set-group-ID or sticky, fails with EPERM. (Linux's vfat turns a mode
    without write bits into the read-only attribute; Radix's FAT keeps
    that attribute as it finds it.)
  - The boot archive's files are user 0's with the modes `ob` wrote.
  - The kernel's own trees (`/devices`, `/processes`, `/kernel`), pipes
    and sockets show Linux's usual modes: 0755 for a directory, 0644 for
    a file, 0660 for a block device, 0666 for a character device, 0600
    for a pipe and 0777 for a socket.
- **The interface.** radix-directory(2) goes to version 6: `open`,
  `make_directory` and `make_node` take a `mode`, and `set_access` (31)
  changes the owner, group and mode of a path, each that is `KEEP`
  staying; a call without a mode is a chown. radix-file(2) goes to
  version 7 with `set_access` (26). `FileStatus` gains `mode`, `owner`
  and `group`. `set_access` needs the write right, as `set_times` does,
  so a read-only capability cannot change what is said of a file; as a
  consequence `fchmod` and `fchown` need a descriptor open for writing
  (EACCES otherwise), which tar, cp and install have when they use them.
- **The C library** passes the modes of `open`, `mkdir`, `mknod` and
  `mkfifo`, fills `st_mode`, `st_uid` and `st_gid` from `FileStatus`,
  and implements `chmod`, `fchmod`, `fchmodat`, `chown`, `fchown`,
  `lchown` and `fchownat`. radix-rt asks for 0666 for a file and 0777
  for a directory, less the mask, as Linux programs ask.
- **The image.** `mkfs.btrfs --rootdir` copies each file's owner from
  the staging copy, which is whoever runs `ob`. When that is not root,
  `ob image` runs it under fakeroot, so the system's files are root's
  on every builder; fakeroot joins the host packages `ob doctor` checks.
  The staging directories get the archive's modes whatever the umask.
- **Not yet.** Nothing checks the modes until 13b.2: `access` still says
  a file that is there may be read, written and run. Users, groups and
  their names come then too, and with them who may chown (root) and
  chmod (the owner).

## Consequences

- `ls -l`, `stat`, `find -perm`, `tar` and `cp -p` see and keep real
  modes and owners; a compiler's output is executable because it asked
  for 0777, not because everything is.
- Files a program makes are 0644 and directories 0755 under the usual
  mask of 022, where they showed 0755 before; a script written to a
  file needs `chmod +x` to look executable, as on Linux.
- The storage boot tests now give files owners and modes on Btrfs and
  ext4 and check them after the reboot, and through Linux's own tools
  (`debugfs` and `btrfs inspect-internal dump-tree`); on FAT and exFAT
  they check what is taken and what is refused. The `root` test checks that the system's
  files are root's.
- A USB stick's files are root's until a disk can be mounted for a
  user; Phase 11's volumed and 13b.2 give mounts an owner, as Linux's
  `uid=` mount option does.
