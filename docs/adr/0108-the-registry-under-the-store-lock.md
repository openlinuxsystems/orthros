<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0108. The object registry is guarded by the capability store's lock

- Status: Accepted
- Date: 2026-10-25

## Context

The capability store names objects; the registry (`user::objects`)
holds what each name stands for. Every change to the registry was made
with the store's lock held, because an object enters it when the store
names it and leaves it when the store releases the name, yet the
registry had a mutex of its own, taken inside the store's. Making an
object and releasing one each took both mutexes: opening and closing a
file took four mutex round trips, eight locked instructions, where one
lock would do. ADR 0059 had already cut the registry's share of making a
file from two holds to one.

## Decision

- The store's mutex guards a `caps::Locked`, the store and the registry
  together. `caps::with` hands it to its closure; it reads as the store,
  and its own `delete`, `destroy_space`, `revoke` and `continue_revoke`
  release objects into the registry it holds, so a released object
  leaves the registry under the lock already held.
- The registry has no lock of its own. The few readers that do not
  already hold the store (`objects::get` for rare lookups, `/proc`'s
  process list) take the store's lock to read it; `kdb`'s census and
  the user tests' check for leaks count the store and the registry in
  one hold, so the two counts are of the same moment.
- A CPU's cache of capabilities fills, as before, with the store
  locked, and now reads the registry under that same hold.

## Consequences

- Opening and closing a file takes two mutex round trips instead of
  four; so does every other object made and released (a pipe's, a
  process's, a frame's).
- The lock order loses a level: nothing is taken between the store and
  the registry any more.
- A reader of the registry now waits for the store's lock, which is held
  longer than the registry's was. The readers are rare and none is on a
  system call's fast path: those find their objects through their CPU's
  cache (ADR 0044).
