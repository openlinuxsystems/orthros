<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0032. IPC is synchronous endpoints with a register-only fast path, seL4-style notifications, and channels built on shared memory

- Status: Accepted
- Date: 2026-10-21

## Context

Phase 5 asks for endpoints with a register-only fast path, notifications
and shared-memory channels. Every service of later phases talks through
them, so they must be fast for small messages, carry capabilities, and
never lose a message or a wake-up when a thread is stopped, a server
exits or an endpoint closes. The L4 family (after Jochen Liedtke's work)
and seL4 showed that synchronous IPC with messages in registers is the
fastest way between address spaces; seL4's notifications showed how to
signal without a message.

## Decision

- An endpoint carries synchronous messages: an info word (label, word
  count, capability count, move and no-block flags) and up to eight data
  words. Whichever of sender and receiver comes second hands the message
  over under the endpoint's lock, copying the words from one thread's
  saved registers to the other's; the first waits in a first-come,
  first-served queue. The receiver gets the sender's badge.
- A call sends and waits for the reply. The thread that receives the call
  holds the caller and alone can reply; receiving again without replying,
  or exiting, fails the caller with `gone`. A caller's IPC state, under
  its own lock, settles the races between a reply, a server letting go,
  and the caller being stopped.
- The fast path: a register-only message never takes the capability
  store's lock beyond the endpoint lookup, and the woken side goes onto
  this CPU's queue (`sched::wake_sync`) when it may run here, so a call
  and its reply on one CPU are two system calls and two switches with no
  interrupt.
- A message carries up to four capabilities when the sender's endpoint
  capability has `grant`: copied (with the source's `copy` right, and
  `transfer` across spaces) or moved into the first empty slots of the
  receiver's receive window, all or nothing.
- Locks are taken in one order: the capability store, an endpoint, a
  thread's IPC state. No lock is held while a thread blocks.
- A notification is a word of bits: `notify` sets the bits of the
  capability's badge (bit 0 without one) and `wait` takes the word,
  sleeping while it is 0. A signal before the wait is kept, so none is
  lost; a ring can wait for one without blocking a thread.
- Shared-memory channels are a library, not a kernel object: two
  programs map one frame holding two single-producer, single-consumer
  queues and sleep on a notification each. The messages never pass
  through the kernel.
- The protocol is modelled in TLA+ (`kernel/ipc/tla/Endpoint.tla`) and
  checked with TLC, as the scheduler's is (ADR 0030); the arithmetic
  (info words, user ranges, notification bits) is the `radix-ipc` crate,
  with host tests and Kani proofs.

## Consequences

- What a small message costs, on one CPU and across two, is measured on
  every merge (ADR 0035), so a change that slows the fast path shows.
- A send blocks until a receiver takes it, so a server cannot be flooded
  with queued messages; asynchronous work goes through notifications,
  channels or rings.
- Endpoints, notifications and queued senders come from the kernel heap
  without a resource-domain charge for now, bounded by a program's
  slots. Phase 6's processes charge their kernel objects to their domain.
- Every endpoint lookup still takes the store's lock, a mutex. A
  per-space read path is the planned fix once many processes call at
  once.
- A change to calls, replies, cancellation or closing should be mirrored
  in the model; the design note maps its actions to the code.
