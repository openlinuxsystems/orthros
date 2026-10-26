<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0117. NVMe shadow doorbells, and virtio requests of 254 pages

- Status: Accepted
- Date: 2026-10-26

## Context

An NVMe driver tells the controller of new commands, and of the
completions it has read, by writing doorbell registers (ADR 0115). In a
virtual machine each register write is an exit to the hypervisor, and
QEMU's controller handles it in its main loop. The specification has a
remedy for emulated controllers, which may say they take it (OACS bit
8): the Doorbell Buffer Config command gives the controller a page of
*shadow doorbells*, which the driver writes in memory, and a page of
*event indexes*, in which the controller says, for each doorbell, the
entry it wants to hear of by register. Linux uses it whenever a
controller offers it.

QEMU's traces of `ob bench` under KVM showed Linux writing no completion
doorbell register for a 1 MiB write (two commands), and Radix writing
two. They showed Linux sending a 1 MiB virtio write as two requests (254
pages and 2), as QEMU's `seg_max` allows, and Radix as three (126, 126
and 4), each a chain QEMU takes from the ring and answers on its own,
though it merges them into one write of its file. On one CI runner type
the 1 MiB write rows were 1.11 (NVMe) and 1.03 (virtio) times Linux;
every other disk row was at or under it.

## Decision

- A controller that says it takes shadow doorbells is given them once it
  has granted its I/O queues and before they are made: two pages of DMA
  memory, kept for the kernel's life, laid out as the doorbell registers
  are (submission queue *y*'s tail at 2*y* strides, its completion
  queue's head one stride on). The admin queue keeps its registers, as
  in Linux.
- A lane moves a doorbell by writing its shadow, then, after a full
  barrier, reading its event index, and writes the register too only if
  the move passed that entry: if the event index is among the entries
  from the old value up to the new one, going round the queue. That is
  the virtio rule (`vring_need_event`); a Kani proof shows the check is
  exactly "the entries passed", counted one by one.
- A reset makes the controller forget them: they are zeroed and given
  again before the queues are made again. A controller that refuses them
  then, or at first, has its lanes write their registers every time.
- The kernel's line for the controller ends in "shadow doorbells" when it
  uses them.
- A virtio request in a table of its own (indirect descriptors) takes up
  to 254 data buffers, in a table of 256 descriptors (4 KiB), within the
  device's `seg_max` and its queue's size less two, as Linux's do; it
  was 126.

## Alternatives

- **Leave the completion head doorbell unwritten.** A lane never has
  more completions outstanding than its queue holds, but the
  specification makes the head doorbell the controller's flow control,
  and a controller may wait on it.
- **Ask the hypervisor for faster doorbells (QEMU's `ioeventfd`).** A
  host setting the guest does not choose, and it still exits.

## Consequences

- Real drives do not offer the command, so nothing changes on hardware.
- Under QEMU, a doorbell is a memory write unless the controller asked
  to hear of it. QEMU's event index for a completion head is the head it
  last heard of, so once a head move goes unannounced the later ones
  stay in memory too, as Linux's do; a submission still rings when the
  controller is idle, since it then asks for the next command.
- A controller that writes nonsense event indexes can keep the driver
  from ringing; its requests go unanswered and the reset follows, as for
  any controller that stops answering.
- Two pages for each controller, never given back (controllers are not
  removed).
- A virtio disk's 64 tables take 256 KiB rather than 128 KiB.
