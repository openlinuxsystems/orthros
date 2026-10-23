<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0091. Named sockets connect through the tree, and descriptors ride on their bytes

- Status: Accepted
- Date: 2026-10-23

## Context

tmux, in the Phase 9 command set, is a server and its clients. The
server listens on a named `AF_UNIX` socket in `/tmp`, each client
connects to that name, and a client sends the server its terminal's
descriptors with `SCM_RIGHTS`, so that the server can draw on the
terminal itself. Many servers that later phases port work the same
way: a name in the tree to connect to, and descriptors passed over the
connection.

Radix has had socket pairs since ADR 0085: two pipes, one each way,
with no names and no way to pass a descriptor. ADR 0085 said that named
sockets would reuse the pair, and that passing descriptors would add a
queue of capabilities beside each byte stream.

Linux lets a process pass any descriptor, sockets among them. A socket
in flight can then hold its own connection open, or two can hold each
other's, and Linux runs a garbage collector over the sockets in flight
to find such cycles and close them.

## Decision

radix-process(2)'s `socket` makes a socket that is neither named nor
connected. radix-directory(2)'s `bind_socket` names it: it makes a file
of the kind `socket` in a directory, which holds what a connecting
socket needs to find the socket while it listens. radix-file(2)'s
`listen` has a named socket take connections, at most a backlog of them
waiting at once (up to 4,096); `connect_socket` connects to a name; and
`accept` takes the connection that has waited longest.

A connection is a socket pair's two pipes. `connect_socket` makes them,
keeps one end, and queues the other at the listening socket, waiting
for room if the queue is full; a non-blocking socket fails with
`EAGAIN` instead, as on Linux. `accept` gives the queued end a socket
of its own. Once the listening socket is gone, the connections still
queued go with it, and connecting to its name fails with
`ECONNREFUSED`; the name stays until it is removed, as on Linux.

A socket keeps its name as the program gave it, which `getsockname`,
`getpeername`, `recvfrom` and `accept` return. Each end of a connection
knows the name and the process at the other end as they were when it
was made: `SO_PEERCRED` gives that process, and user and group 0 until
Phase 13 brings users. Linux's abstract names, which start with a NUL,
are not taken (`EOPNOTSUPP`). Names live on tmpfs; on FAT, `bind` fails
with `EPERM`, as on Linux.

radix-file(2)'s `send_files` sends up to 253 files or directories with
a write's bytes, as many as Linux's `SCM_MAX_FD`. They ride on its
first byte, in a queue beside the pipe's buffer. A read never goes past
a byte that has files on it unless it starts there, so the files come
out with the bytes they were sent with, in order; `receive_files` puts
them in the reader's lowest free descriptors, with the rights the
sender had, and closes any it has no room for, which `MSG_CTRUNC`
reports. A plain read closes the files it passes. At most 1,024 files
are on their way each way at once, after which a send fails with
`ETOOMANYREFS`.

A socket cannot be sent: `send_files` fails with `EOPNOTSUPP`. No file
on its way can then hold a socket, so no socket can keep itself or
another alive through a connection, and there are no cycles to find.
The kernel needs no collector.

The kernel gains the errors `refused` (`ECONNREFUSED`), `connected`
(`EISCONN`), `not_connected` (`ENOTCONN`), `not_supported`
(`EOPNOTSUPP`) and `too_many` (`ETOOMANYREFS`). The process, directory
and file interfaces go to version 4.

## Consequences

- tmux's server and clients work as they do on Linux. The `socket`
  boot test checks names, connections, the backlog, connections from
  another process, and descriptors passed one way and the other.
- A read on Linux reads on past bytes that brought descriptors, and
  stops after them; on Radix it stops before them. A program that
  reads a stream piece by piece, as every one must, sees no
  difference.
- A program that passes a socket, as some servers do to hand a
  connection to a worker, fails until a port needs it. Allowing it
  then means a collector, or a rule that keeps cycles out.
- The pipe's write and read look at the queue of files only when it
  has some, so pipes and pairs that pass none cost what they did.
- Datagram and sequenced-packet sockets, `MSG_PEEK`, credentials as
  control messages, and permission checks on connecting (every process
  runs as root until Phase 13) are still to come, along with socket
  names on Btrfs (Phase 10) and network sockets (Phase 12).
