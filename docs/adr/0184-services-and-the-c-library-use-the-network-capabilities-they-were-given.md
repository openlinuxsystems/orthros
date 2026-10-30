<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0184. Services and the C library use the network capabilities they were given

- Status: Accepted
- Date: 2026-10-30

## Context

ADR 0183 makes the network a capability: a range of ports, whose send
right connects out and whose receive right binds and listens there, in
the eight slots from `SLOT_NETWORK`. Two pieces are left. Spark has to
decide what each service gets, and the C library's BSD sockets, which
every port is written for, have to work with whatever a program was
given. A POSIX program does not know about slots, so it cannot say
which capability a call should go through, and Linux programs expect
`EACCES` (or `EPERM`) when a policy forbids a call, not a new error.

## Decision

- **A service file says what network its service gets** (spark(8)).
  `network.outbound = yes` gives it a capability with the send right for
  every port; `network.listen = 22, 8000-8009` gives it one with the
  receive right for each range. Nothing is given by default. They go in
  the slots from `SLOT_NETWORK` in that order, at most eight, with the
  copy and transfer rights so that they last through `fork` and `exec`.
  These are the manifest's `network.outbound` and `network.listen`, for
  the system's services now and for apps when the store installs them.
- **The C library tries each network capability in turn.** `socket`,
  `bind`, `connect`, and `sendto` or `sendmsg` to an address make the
  call through the first slot, then the next while the kernel refuses
  because the slot is empty, holds no network capability, lacks the
  right or does not cover the port; the first other answer is the
  call's. When every slot refused, the call fails with `EACCES`.
- **`listen` on an unbound socket binds it first**, through the first
  capability with the receive right, as Linux binds such a socket to a
  port of its own choosing. Without one, `EACCES`.
- **The C library asks a socket's family only when it matters.** The
  kernel's `send`, `receive`, `read` and `write` are the same calls for
  both families, so `send`, `recv`, `read` and `write` make one system
  call as before; only calls with an address, control messages, or more
  than one buffer ask the socket's options whether it is a network
  socket first, so a Unix socket's send and receive cost what they did.

## Consequences

- What a service may do on the network is in its file, where an
  administrator reads it; a service that asks for nothing cannot reach
  the network, whatever it runs.
- A program can narrow what it holds itself, by deriving a capability
  for fewer ports or rights and deleting the others, as the `inet` test
  does; the C library needs no setting for it.
- A call another capability would allow is tried through each before
  it: at most eight refused system calls, only when a program holds
  several capabilities and the first does not fit.
- `listen` on an unbound socket through a capability with the receive
  right for a range other than the ephemeral ports fails with `EACCES`,
  where Linux would listen on a port nothing asked for. A program that
  listens names its port.
- The `inet` test in the `posix` boot test checks TCP and UDP through
  the C library, and that a program given no network capability gets
  `EACCES`. The `spark-network` boot test checks that a service gets
  the capabilities its file asks for and no others.
- Spark starts every service with `exec`, which loads a program on the
  C library (radix-posix(7)); radix-rt's native programs do not start
  that way yet, so the test services are C programs.
