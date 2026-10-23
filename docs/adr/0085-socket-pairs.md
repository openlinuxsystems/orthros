<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0085. Socket pairs are two pipes, one each way

- Status: Accepted
- Date: 2026-10-23

## Context

Nukesh's line editor needs `socketpair` (ADR 0083). Its terminal
library, crossterm, uses mio's `UnixStream::pair` to wake its event
loop from a signal handler when the terminal is resized. Rust's
`UnixStream::pair`, Python's `socket.socketpair`, and many servers and
tools use the same call to talk between a process and its child, or
between threads. Until now the C library had no sockets, and
`socketpair` failed with `ENOSYS`.

Named `AF_UNIX` sockets, and passing descriptors over them, come later
in Phase 9 for tmux. Network sockets come with Phase 12. A pair needs
neither names nor a network stack: it is a byte stream each way between
two ends.

## Decision

radix-process(2) gains `socket_pair`. It puts two connected ends in two
slots, with the same flags as `pipe`. radix-file(2) gains `send` and
`receive`, which take the flags `dont_wait` (`MSG_DONTWAIT`) and
`no_signal` (`MSG_NOSIGNAL`), and `shutdown`. Files gain the kind
`socket`, and the kernel gains the error `not_socket` (`ENOTSOCK`).

In the kernel, a pair is two pipe buffers, one each way. Each end reads
one and writes the other, so a pair keeps everything a pipe does:

- 64 KiB a way, and writes of up to 4 KiB that are never split;
- reads that wait for data and give 0 once the other end has stopped
  writing;
- writes that wait for room, and fail with `EPIPE` and raise `SIGPIPE`
  (unless `no_signal`) once the other end has stopped reading;
- `poll`, with the events Linux gives for a stream socket.

`shutdown` lets go of an end's part of either pipe or both, once for
the whole socket, so it acts on every descriptor of the socket, as in
POSIX. Ends live on the pipes' detached filesystem, so they are copied,
passed to children and closed on exec like a pipe.

The C library maps the socket calls onto this. It gives `socketpair`
for `AF_UNIX` with `SOCK_STREAM`, `SOCK_NONBLOCK` and `SOCK_CLOEXEC`,
and `send`, `recv`, `sendto`, `recvfrom`, `sendmsg` and `recvmsg`. Of
the flags it takes `MSG_DONTWAIT`, `MSG_NOSIGNAL` and `MSG_WAITALL`,
and it ignores `MSG_MORE`. It also gives `shutdown`, `getsockname` and
`getpeername`, which say the ends have no name, and the basic socket
options (`SO_TYPE`, `SO_DOMAIN`, `SO_ERROR`, the buffer sizes).

Some calls fail for now:

- datagram and sequenced-packet pairs fail with `ESOCKTNOSUPPORT`;
- `MSG_PEEK`, `MSG_OOB` and control messages fail with `EOPNOTSUPP`;
- `socket` fails with `EAFNOSUPPORT`.

## Consequences

- Nukesh's line editor, and every Rust program that uses
  `UnixStream::pair` or mio, works on Radix. The `socket` C test and
  the Rust standard library test check pairs at every boot.
- The pipe's buffer, waiting and wake-up code serves both, so a fix to
  one is a fix to the other.
- The file interface and the process interface each go to version 3.
- Named sockets will reuse the pair: a connection that a listening
  socket accepts is a pair. Passing descriptors will add a queue of
  capabilities beside each byte stream, and `SCM_RIGHTS` will fill it.
