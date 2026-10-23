<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0076. Programs set a file's times; reading never changes them, and each mount has a device number

- Status: Accepted
- Date: 2026-10-22

## Context

The kernel kept two times for a file, when it was made and when its
contents last changed, and set them itself. `utimensat` and `futimens`
did nothing and said they had succeeded, `stat` gave the change time as
the last read too, and every file had the device number 1. Tools rely on
all three: `touch`, `cp -p`, `tar` and `rsync` set times, `make`
compares them, and `find`, `du`, `cp -r` and `tar` tell filesystems
apart and find hard links by (`st_dev`, `st_ino`). libc-test's
functional/utime checks that times set are kept. Phase 9 brings those
tools.

Linux updates a file's last read time on reads, by default only once a
day or when the file changed after it (`relatime`); many systems mount
with `noatime`, which never does, since the update turns reads into
writes. Linux's vfat keeps the change time in two-second steps and the
read time to the day.

## Decision

- The VFS keeps a **read time** beside the change time. A new file's is
  when it was made; **reading never changes it**, as with Linux's
  `noatime`.
- radix-file(2)'s **`set_times`** sets a file's read and change times,
  in nanoseconds since 1970, or to now (`TIME_NOW`), or leaves either
  (`TIME_OMIT`); radix-directory(2)'s does so for a path. The file's
  capability needs the write right, the directory's for a path too.
  tmpfs keeps the times exactly; FAT as Linux's vfat does, and what
  `stat` says straight after is what the disk will say. Devices and
  pipes keep no times and refuse with `not_permitted` (`EPERM`); a
  read-only mount with `read_only` (`EROFS`).
- `stat` says the **mount** a file is on, a number unique since boot,
  which the C library gives as `st_dev`.
- The C library's `utimensat` and `futimens` take times before 1970 as
  invalid (`EINVAL`), since the kernel's times count from 1970.

## Consequences

- `touch`, `cp -p`, `tar` and Rust's `File::set_times` work; the `files`
  C test and libc-test's functional/utime check it, and unit tests check
  FAT's rounding across a remount.
- Programs that read `st_atime` to see whether a file was read since it
  changed, such as mail notifiers, see that it never was. That is the
  price `noatime` systems pay too.
- A descriptor opened only for reading cannot set times, where Linux
  lets the file's owner; users come in Phase 13, and the rule can widen
  then.
- The kernel keeps no time of the last change to a file's status, so
  `st_ctim` is `st_mtim`; a filesystem that keeps one (Btrfs, Phase 10)
  can add it to `stat`.
- `statvfs` reports how many files and directories a filesystem can
  hold. A tmpfs now holds at most one for every two pages of memory,
  Linux's default, and making one more fails with `no_space`, so that
  empty files cannot use up the kernel's heap; the boot archive holds
  the ones it has. FAT keeps no count, as Linux's vfat reports none.
