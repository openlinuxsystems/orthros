systools(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

systools - Dukovo's administration tools, in one program

# SYNOPSIS

_tool_ [_argument_ ...]

*systools* _tool_ [_argument_ ...]

*systools --list*

# DESCRIPTION

systools is the program behind the commands an administrator expects
from Linux whose Linux versions read Linux's _/proc_, its utmp file or
its system calls, which Radix does not have: Dukovo has its own, written
in Rust for Radix, with the Linux tools' names, options and output, so
that what people type and what scripts read stay the same.

Each command is a link to _/system/commands/systools_ by its name, and
systools runs the tool its name says. *systools* _tool_ runs a tool by
name, and *systools --list* lists them.

The tools that show processes read *radix-proc*(5); *lsof*(8) lists the
capabilities each process holds; *rtrace*(1) reads a trace of a
process's system calls (*radix-trace*(2)); *dmesg*(1) reads the kernel's
log through _/devices/kmsg_ (*radix-files*(7)), and *logd*(8) keeps it
as the system's log and its audit log, which *log*(1) and
*journalctl*(1) read and *logtrim*(8) trims; *notifyd*(8) passes the
notifications *notify-send*(1) sends to the console, and
*notifications*(1) lists them; *ip*(8) and *ss*(8) read the network's
interfaces and sockets from *radix-sysfs*(5) and *radix-proc*(5), and
*ip*(8) sets the interfaces up through the network capability
(*radix-network*(2));
*shutdown*(8), *reboot*(8), *poweroff*(8), *halt*(8) and *service*(8)
ask Spark over its control socket (*spark*(8)); *authd*(8) keeps the
accounts, which *user*(8) makes and lists and whose passwords
*passwd*(1) changes, and starts the sessions of those who sign in with
*login*(1). The user columns show the user each process acts
for (*radix-names*(2)).

A mistake in the command line makes a tool fail with exit status 2 (1
for those whose Linux versions use 1), after a message on standard
error that starts with the tool's name.

# TOOLS

[[ *Tool*
:- *What it does*
|  *balance*(8)
:  move a Btrfs filesystem's chunks, converting them to another profile
|  *authd*(8)
:  keep the accounts and check passwords
|  *blkid*(8)
:  what each disk and partition holds: filesystems, labels and UUIDs
|  *chgrp*(1), *chmod*(1), *chown*(1)
:  change a file's group, mode or owner
|  *column*(1)
:  put lines in columns, or a table
|  *df*(1)
:  the space of each filesystem
|  *device*(8)
:  add disks to a Btrfs filesystem and remove them
|  *diskhealth*(8)
:  what each disk says of its health: SMART and the NVMe health log
|  *dmesg*(1)
:  the kernel's log
|  *ectool*(8)
:  set the embedded controller's fans, fan curves, LEDs and ARGB header
|  *eject*(1)
:  unmount a volume so that its disk can be pulled out
|  *findmnt*(8)
:  the mounts, as a tree, or the one sought
|  *fstrim*(8)
:  discard what a mounted filesystem does not use
|  *free*(1)
:  memory in use and free
|  *halt*(8), *poweroff*(8), *reboot*(8)
:  halt, power off or restart the machine
|  *hexdump*(1), *hd*(1)
:  a file's bytes, in hexadecimal or other formats
|  *hwinfo*(8)
:  what the machine is, as its firmware's SMBIOS tables say
|  *ionice*(1)
:  a process's I/O class and level
|  *iostat*(1)
:  what the CPUs and the disks have done, per second
|  *ip*(8)
:  the network interfaces, their addresses and routes, and set them
|  *journalctl*(1), *log*(1)
:  the system's log
|  *keymap*(1)
:  the keyboard layout the consoles type in
|  *kill*(1)
:  send a signal
|  *logd*(8)
:  keep the system's log
|  *login*(1)
:  sign in at a console
|  *logtrim*(8)
:  trim the system's log
|  *lsblk*(8)
:  the disks and their partitions, as a tree
|  *lscpu*(1)
:  what the processor is
|  *lsmem*(1)
:  the machine's RAM in memory blocks
|  *lsof*(8)
:  what each process holds open
|  *lspci*(8)
:  the PCI functions
|  *lsusb*(8)
:  the USB devices, or the tree of their buses and ports
|  *mkswap*(8)
:  make a swap area on a disk or partition
|  *notifications*(1)
:  the notifications notifyd keeps
|  *notify-send*(1)
:  send a notification
|  *notifyd*(8)
:  pass notifications to the screen
|  *nvme*(8)
:  the NVMe namespaces, and a drive's SMART / Health Information log
|  *passwd*(1)
:  change a password
|  *pgrep*(1), *pkill*(1)
:  find or signal processes by name
|  *pidstat*(1)
:  what each process uses
|  *ps*(1)
:  the processes
|  *quota*(8)
:  turn a Btrfs filesystem's simple quotas on and off, show and limit them
|  *renice*(1)
:  a process's nice level
|  *replace*(8)
:  put a new disk in the place of one of a Btrfs filesystem's
|  *rtrace*(1)
:  trace a process's system calls
|  *script*(1)
:  record a terminal session
|  *sensors*(1)
:  the machine's temperatures and fan speeds
|  *service*(8)
:  start, stop and look at the services
|  *shutdown*(8)
:  stop the system, now or later
|  *ss*(8)
:  the TCP and UDP sockets
|  *scrub*(8)
:  read and check every block of a Btrfs filesystem
|  *subvolume*(8)
:  make, snapshot and remove Btrfs subvolumes
|  *swapon*(8), *swapoff*(8)
:  turn swap areas on and off, and list them
|  *taskset*(1)
:  the CPUs a process may run on
|  *top*(1)
:  the processes, refreshed
|  *tree*(1)
:  directories as a tree
|  *uptime*(1)
:  how long the system has been up
|  *user*(8)
:  make and list accounts
|  *users*(1), *w*(1), *who*(1)
:  who is logged in, and what they are doing
|  *vmstat*(8)
:  memory, CPU and disk activity
|  *volumed*(8)
:  mount the volumes plugged in after boot by their labels
|  *watch*(1)
:  run a command again and again
|  *watchdog*(8)
:  the hardware watchdog Spark pets
|  *which*(1)
:  where a command is
|  *xxd*(1)
:  a hex dump, and back

# SEE ALSO

*radix-proc*(5), *radix-posix*(7), *nukesh*(1), *spark*(8)
