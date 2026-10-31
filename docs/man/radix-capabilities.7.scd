radix-capabilities(7) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

radix-capabilities - capabilities, rights, badges, sealing, delegation
and revocation in the Radix kernel

# DESCRIPTION

Every resource in Radix is reached through a capability: an unforgeable
token that names one kernel object and the rights its holder has over it.
A program holds capabilities in slots of its capability space and names
them by slot number; it never sees or makes a capability itself. To give
another program access, it hands over a copy, perhaps with fewer rights,
and it can later take back everything it handed out by revoking.

This page describes the model as of Phase 5 of the roadmap. Programs
use their capabilities through system calls: the methods every
capability has, such as *derive*, *move_to* and *revoke*, are in
*radix-capability*(2), and the others with the kind they work on (see
*radix-ipc*(7)). The design, with the argument that capabilities cannot
be forged or widened, is in _docs/design/capabilities.md_.

# OBJECT KINDS

A capability names one object of one of these kinds. Each kind has the
rights listed, plus *copy* and *transfer*, which every kind has.

*memory*
	A block of physical memory that other objects are made from (see
	*RETYPING*). Rights: *read*, *write*, *execute*, *control*.

*frame*
	A block of memory that can be mapped. Rights: *read*, *write*,
	*execute*.

*address space*
	A set of mappings that threads run in. Rights: *read*, *control*.

*thread*
	A thread of execution. Rights: *read*, *control*.

*process*
	A process, with its threads and capability space. Rights: *read*,
	*control*, *signal*.

*endpoint*
	A message channel; also the key that seals and unseals capabilities
	(see *SEALING*). Rights: *send*, *receive*, *grant*, *seal*, *unseal*.

*notification*
	A word of flags that can be signalled and waited for. Rights: *send*,
	*receive*.

*IRQ*
	A range of interrupt lines (at most 256). Rights: *receive*,
	*control*.

*I/O ports*
	A range of x86 I/O ports (at most 65,536). Rights: *read*, *write*.

*MMIO*
	A range of pages of device registers (at most 2^40). Rights: *read*,
	*write*.

*DMA*
	A block of memory a device may read or write. Rights: *read*,
	*write*.

*domain*
	A resource domain: a CPU weight and a memory limit shared by the
	threads in it, and its class for the out-of-memory policy (see
	*radix-scheduling*(7)). Rights: *read*, *control*.

*random*
	The kernel's random number generator, which every program receives
	at start (see *radix-random*(2)). Rights: *read*.

*ring*
	A submission and completion ring shared with a program (see
	*radix-ring*(2)). Rights: *control*, *send*.

*kernel log*
	Permission to write lines to the kernel log (see
	*radix-kernel-log*(2)). Rights: *write*.

*file*
	An open file description: a file, a pipe's end, a socket or a
	device (see *radix-file*(2)). Rights: *read*, *write*, *bypass*.

*directory*
	A place in the file tree, beneath which paths are resolved (see
	*radix-directory*(2)). Rights: *read*, *write*, *control*,
	*bypass*. On a view, a directory made of attached capabilities
	(ADR 0215), *control* is the right to attach to it.

*power*
	The machine's power, which Spark turns off or restarts (see
	*radix-power*(2)). Rights: *control*.

*network*
	A range of the network's ports (at most 65,536), to connect out
	through and to listen on (see *radix-network*(2)). Rights: *send*,
	to connect out to any address and send datagrams; *receive*, to bind
	to and listen on the ports in the range.

*platform*
	The machine's platform controls, which its embedded controller
	keeps: its fans' duties and curves, its LEDs and its ARGB header
	(see *radix-platform*(2)). Rights: *read*, to ask what they are;
	*control*, to change them.
*names*
	The authority to give processes and capabilities the names they act
	for, which only Spark holds (see *radix-names*(2)). Rights:
	*control*.

# RIGHTS

The thirteen rights are *read*, *write*, *execute*, *send*, *receive*,
*grant*, *signal*, *control*, *seal*, *unseal*, *copy*, *transfer* and
*bypass*. What the first ten allow depends on the kind and is described
with the operations that need them. *bypass*, on a directory or a file,
passes the owners, groups and modes of the files reached through it, as
Linux's CAP_DAC_OVERRIDE does but only beneath that directory, and is
carried to what is opened through it (*radix-names*(2), ADR 0214). Two
concern capabilities themselves:

*copy*
	Needed to derive, seal or retype: to make anything from this
	capability. A capability without it can be used and moved, but not
	copied.

*transfer*
	Needed to place anything made from this capability in another
	capability space, or to move it there. A capability without it stays
	in its space.

The original capability of a new object has every right of its kind. A
use of a capability succeeds exactly when the capability is not sealed,
names an object of the kind the operation works on, and holds every
right the operation needs; otherwise the operation fails with
*WrongKind*, *MissingRights* or *Sealed* (see *ERRORS*).

# DERIVING

Deriving copies a capability into an empty slot, keeping only the rights
in a mask, and optionally:

- adding a *badge*, a non-zero 64-bit number, to an *endpoint* or
  *notification* capability that has none. A badge is set once: a
  badged capability and everything derived from it carry the same badge.
  A server tells its clients apart by the badges on their capabilities.
- narrowing a range (*IRQ*, *I/O ports*, *MMIO*) to a part of itself,
  given as an offset and a length inside the range.

The copy holds the original's rights and the mask's, and no others. It
becomes the original's child in the derivation tree (see *REVOCATION*).
Nothing can add a right, widen a range, change a badge or unseal a
capability without its key, so no sequence of operations gives anyone
more than what they started with.

# SEALING

Sealing makes a capability that can be passed around but not used or
changed until it is unsealed. It needs an *endpoint* capability with the
*seal* right as the key, and *copy* on the capability. The sealed copy
cannot be used, sealed again, retyped or derived from, except as an exact
copy (with *copy*). Unsealing needs a capability to the same endpoint
with the *unseal* right, and gives back exactly the capability that was
sealed. A server can hand a client a sealed capability as an opaque
handle, and recognise it when it comes back.

# RETYPING

Retyping makes new objects from a *memory* object: *frame*, *DMA* or
smaller *memory* objects, each of 2^_order_ pages, up to 1,024 at once.
It needs *control* and *copy* on the memory capability; the new
capabilities have the memory capability's rights that their kind has.
The memory object hands out blocks in order, each aligned to its size,
from a watermark that only rises, so the objects made never overlap.
When every object made from it is gone, the memory object can be retyped
again from the start; when its last capability and every object made
from it are gone, its pages go back to the kernel's frame allocator.

# REVOCATION

Every capability made from another is its child in the derivation tree,
which spans all capability spaces. Deleting a capability deletes only
that one: its children move up to its parent. Moving a capability to
another slot keeps its place in the tree. So the capabilities below a
capability in the tree are always exactly those still existing that were
made from it, directly or not, however they have been moved, copied on
or had their intermediaries deleted.

Revoking a capability deletes every capability below it, in every
space, and keeps the capability itself. A large revoke works in steps,
letting other CPUs in between; while it runs, nothing can be derived from
the capability or from any capability below it (*Revoking*), and the
capability cannot be moved. Every revoke ends.

An object lives while any capability names it, and while anything
retyped from it lives. Deleting the last capability destroys the object.

# LIMITS

- A capability space has at most 65,536 slots.
- Derivations are at most 64 deep: a capability 64 levels below an
  original cannot be derived from (*TooDeep*). Deleting capabilities in
  between does not lower this count.
- A retype makes at most 1,024 objects.
- Names of spaces and objects carry a generation number, so a name that
  outlives its space or object is refused rather than reaching the next
  one to use its place.

# ERRORS

Operations on capabilities fail with one of these, and change nothing:

*NoSuchSpace*, *NoSuchSlot*
	The space does not exist (any more), or the slot number is past its
	end.

*Empty*, *Occupied*
	The slot to use holds no capability, or the slot to fill holds one.

*WrongKind*
	The capability names an object of another kind than the operation
	needs.

*MissingRights*
	The capability lacks a right the operation needs.

*Sealed*, *NotSealed*, *WrongKey*
	A sealed capability was used or changed; a capability that is not
	sealed was unsealed; or the key is not the endpoint it was sealed
	with.

*NotBadgeable*, *AlreadyBadged*, *InvalidBadge*
	A badge was asked for on a kind that carries none, on a capability
	that has one, or was 0.

*NotARange*, *OutOfRange*
	A range was asked of a kind that has none, or reaches outside the
	capability's range.

*TooDeep*, *Revoking*
	The derivation would go deeper than 64, or a revoke is under way on
	the capability or above it.

*Exhausted*, *NoMemory*, *InvalidArgument*
	The memory object has no room for the blocks asked for; the kernel
	is out of memory; or an argument is invalid, such as retyping into a
	kind that is not made from memory.

# DIAGNOSTICS

At boot the kernel reports its store:

```
Capabilities: ready; 21 object kinds, 13 rights, spaces of up to 65536 slots, derivations up to 64 deep
```

The *kdb* command *caps* counts the spaces, capabilities and objects by
kind, and the *caps* self-test checks the whole model on every CPU; both
are described in *radix-debug*(7).

# SEE ALSO

*radix-ipc*(7), *radix-capability*(2), *radix-names*(2), *radix-errors*(2), *radix-debug*(7),
*dukovo-boot*(7), *ob*(1)
