<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Storage: files, disks and FAT

This note describes Radix's storage as of Phase 7, with the disks and
the start of Btrfs of Phase 10: the virtual file system and its
capability-relative paths, directory and file objects, the page cache
and the block cache, the block layer, the disk queue and its virtio, AHCI
and NVMe drivers, raw access to block devices, FAT, Btrfs, tmpfs and the
generated trees, writeback and
reclaim, and the hierarchy Spark builds. It ends with the semi-formal
arguments that paths never leave their directory and that freed
clusters are never written by their old owner, the lock order, and the
known limits. The user-facing descriptions are radix-directory(2),
radix-file(2), radix-block(2), radix-files(7), radix-proc(5),
radix-sysfs(5) and spark(8); the decisions behind the design are ADRs
0052 to 0059, 0111, 0112, 0115, 0117, 0125, 0127 and 0128.

## Crates

```text
kernel/lib/hw           radix-hw: device registers (Mmio), DMA memory (Dma),
                        pages devices read and write (Page), MSI messages,
                        PCI configuration space through ECAM (pci.rs)
kernel/drivers/virtio   radix-virtio: the modern PCI transport, split
                        virtqueues, virtio-blk (compartment)
kernel/drivers/ahci     radix-ahci: AHCI controllers and their SATA
                        disks (compartment, Phase 10)
kernel/drivers/nvme     radix-nvme: NVMe controllers, their queue pairs
                        and namespaces (compartment, Phase 10)
kernel/fs/block         radix-block: block devices, MBR and GPT, the block
                        cache, the page index (pagemap), RAM disks for tests
                        (compartment)
kernel/fs/vfs           radix-vfs: nodes, filesystems, paths, the mount
                        table, the page cache, tmpfs, generated files, the
                        boot archive (compartment)
kernel/fs/fat           radix-fat: FAT12/16/32 with long names (compartment);
                        tla/Clusters.tla, the model of cluster reuse
kernel/fs/btrfs         radix-btrfs: Btrfs's checksums, superblock, chunks,
                        trees, files and subvolumes, read-only
                        (compartment, Phase 10b)
kernel/radix            fs/: the tree, disks and their queue, devfs,
                        procfs, sysfs, the writeback thread; user/files.rs:
                        the directory and file methods
abi                     directory.ridl, file.ridl, block.ridl
userspace/radix-rt      fs.rs: Dir and File for programs
userspace/spark         Spark: the hierarchy and /volumes (since v1)
userspace/storage       the `storage` boot test's program
tools/xtask             frames.rs: the stack frame budget; test.rs: disks,
                        a second boot, and the host checks with dosfstools
                        and mtools
```

A *compartment* is a crate with `#![forbid(unsafe_code)]` that builds and
tests on the host. Every storage crate but `radix-hw` is one, and
`radix-hw` hands out device memory only as types whose accesses are
checked, so a driver or filesystem bug can corrupt its own data but not
the kernel's.

## The tree

Every filesystem shows its files as nodes (`radix_vfs::Node`): a
directory looks names up, creates, removes, renames and lists; a file
reads and writes bytes at offsets, and can be cut or grown; both have a
status (kind, size, bytes allocated, an id, times). A `FileSystem` is a
tree of nodes with a root, and can sync, shrink its caches and say how
full it is.

The mount table (`radix_vfs::mount::Vfs`) joins filesystems into one
tree. A mount records its filesystem, its root node, the mount and node it
covers, whether it is read-only, its path from the root (for
`/processes/mounts`) and where it was mounted from. A place in the tree is a
`Location`: a mount and a node of it.

### Paths

A path is UTF-8, at most 4,096 bytes, with no NUL, and names of at most
255 bytes. It is never absolute: a leading `/` is refused with `outside`.
Resolution walks it from the directory it came with, keeping the chain of
places it went through:

- a name is looked up in the last place, and if something is mounted on
  what it finds, the walk moves to that mount's root (and up a stack of
  mounts on the same directory);
- `.` stays; `..` pops the last place, and popping the first one is
  refused with `outside`.

So `..` always returns to a place this walk entered, which is how it
crosses back over a mount point, and never reaches anything above the
starting directory. A path with no `..`, nearly every path, needs no
chain: the walk holds only where it is, takes no reference to the places
it passes, and allocates nothing; at the first `..` it starts again from
the directory with the chain.

Each mount keeps a 128-bit filter of its nodes that something is mounted
on, one bit each, chosen by the node's address (Fibonacci hashing), as a
Bloom filter with one hash. A walk past a node whose bit is clear knows
that nothing is mounted there without taking the table's lock, which is
the case for every node of a filesystem with nothing mounted on it; a set
bit sends it to the table. Linux marks mount points in its directory
cache (`DCACHE_MOUNTED`) for the same reason. Bits change only under the
table's lock: `mount` sets the bit before the mount is in the table, so a
walk that misses the bit has missed the mount too, and `unmount`
recomputes the filter of the mount below after the mount has left, each
word whole, so no bit a mount still needs is ever clear.

### Mounting

`mount` needs the control right. It resolves the path, checks that it is
a directory, makes the filesystem (`crate::fs::make`: `tmpfs`, `fat` on a
block device the caller names by a file capability, or the generated
`proc`, `sys`, `dev` and `bootfs`), and adds the mount. A second mount on
the same place stacks on the first, as on Linux. `unmount` takes off the
mount whose root the path names, after syncing it, unless something is
mounted on it or anything holds a place on it: the table and the call
hold two references to the mount, and any more is a handle. A rename
never crosses mounts, and a mount point cannot be removed.

## Directory and file objects

A directory or file capability names a `FileObject`: a `Location`. Its
methods are in `user/files.rs`, decoded by the generated `*Args` types,
with paths copied from the caller once (`Path::copy`) and checked as
UTF-8 there. The capability the call names is looked up once, by the
system call, and handed to the method, which checks its kind and rights
before anything is looked up. An `open` that creates or cuts a file
checks the destination slot is empty first, so that nothing changes for
a call that fails; any other `open` finds an occupied slot when it puts
the capability there, and the object it made goes once the store's lock
has gone.

`open` hands on at most the rights of the directory it opens through: a
file gets read and write, a directory read, write and control, both copy
and transfer. Opening a file for writing on a read-only mount fails with
`read_only`.

Releasing a file object can mean freeing a removed FAT file's clusters,
which reads and writes the disk. The capability store releases objects
with its lock held, so file objects are queued (`fs::defer_drop`, linked
through the objects themselves, so queueing needs no memory) and dropped
once the store's lock has gone (`fs::drop_deferred`, from `caps::with`).
An object whose filesystem says that its nodes only free memory when they
go (`FileSystem::light_drops`: tmpfs, the boot archive and the generated
trees) is dropped at once instead (`fs::release`). Only its node can go
with it: no mount is unmounted while a handle holds a place on it.

## The page cache

A file's data lives in its `FileCache`: its size, and the pages of it in
memory, indexed by page number in a `PageMap`, a radix tree of 64-slot
nodes like Linux's XArray (a lookup takes six bits of the page number per
level and never compares keys). Each cached page records whether it is
dirty.

- A read copies from the pages to the caller, fetching missing pages
  through the file's `Backing` first. Bytes past the end read as nothing;
  a page never written reads as zeros.
- A write copies into the pages and marks them dirty, growing the file
  first (`Backing::reserve`) if it ends past the end. The bytes between
  the old end and the new data are zeroed, in memory and on the disk
  (`Backing::zero`), since the disk may hold anything there.
- Cutting a file drops the pages wholly past the new end, dirty or not,
  zeroes the rest of the last page, and then has the backing free the
  space (`Backing::truncate`).
- `sync` writes the dirty pages back through the backing; a page whose
  write fails stays dirty.
- `shrink` drops clean pages.

One mutex guards a file's pages and size, and is held while the disk
works: a file has one request in flight at a time.

Copies between the caller's memory and a page are the only copies: the
disk reads and writes the page itself by DMA, so data moves once between
a program and the disk. Pages come from the frame allocator, owned by
`PageCache`, so `/processes/meminfo` counts them.

## The block layer and the block cache

A `BlockDevice` is a disk or a part of one, in 512-byte sectors, as
Linux counts them, whatever the size of the disk's own (ADR 0123). A
disk of 4 KiB sectors says so in its description (`logical_block`), and
each request to it must start and end on one of them. A
request reads or writes sectors from one place into a list of
`Segment`s, each a range of a page, and blocks the calling thread until
the device is done. `partition` reads MBR and GPT tables (the GPT's CRC32
checked, the backup header when the primary is bad) and `Slice` is a
partition as a device of its own. Tables count in the disk's own
sectors, as their specifications say, and partitions in 512-byte ones.
As on Linux, a bad entry costs only
itself: one that starts past the disk's end is skipped, an MBR one that
reaches past it is cut to fit (and logged so), a broken chain of logical
partitions ends where it breaks, and an extended partition shows only
its first two sectors (the first one, on a disk of 4 KiB sectors), so
that nothing made on it can
wipe the logical ones.

A filesystem's metadata goes through a `BlockCache` of its device: page
`n` holds the device's bytes from `n * 4096`, with a bit per sector for
*valid* (read or written) and one for *dirty*. A read fetches only the
sectors it lacks; a write of whole sectors fetches nothing; on a disk of
4 KiB sectors, each bit's sector is a part of one of them, and the cache
fetches, writes and forgets whole ones; `sync` writes
the dirty sectors in runs and then flushes the device. `discard` forgets
sectors exactly, without touching their neighbours in the same page, and
`shrink` drops clean pages, which the cache also does on its own past
4,096 pages.

## Disks

At boot, `fs::disk::init` walks every PCI segment the ACPI MCFG table
describes (`radix_hw::pci::scan`: every bus from the segment's first,
through PCI-to-PCI bridges, with each function's BARs sized), logs each
function, and binds the disks it has drivers for: virtio block devices
(`vda`, `vdb` and on) and, since Phase 10, the SATA disks of AHCI
controllers (`sda`, `sdb` and on) and the namespaces of NVMe controllers
(`nvme0n1`, `nvme0n2`, `nvme1n1` and on). It then reads each disk's
partition table and names each partition as Linux would (`vda1`, `sda1`,
and `nvme0n1p1` after a name that ends in a digit), for `/devices` and
`/kernel/block`.

Binding a virtio block device:

1. maps the BARs the virtio capabilities name, uncached, and turns on
   bus mastering;
2. sets the device up through the modern transport (ADR 0053): reset,
   the features it uses (discard among them since Phase 10), one split
   virtqueue in DMA memory;
3. asks the architecture for an interrupt vector and programs it as the
   queue's MSI-X entry (ADR 0052); a device with no vector to spare is
   polled by the threads that wait for it.

Binding an AHCI controller (`radix-ahci`, ADR 0112) maps its registers
(BAR 5), takes it from the firmware, resets it into AHCI mode, gives it
one MSI, and resets every port's link at once. Each port with an ATA
disk on it is identified (its size, sector sizes, queue depth, write
cache, FUA and TRIM) and becomes a disk. A controller that cannot reach
memory above 4 GiB is refused, and ATAPI devices and port multipliers
are logged and left alone.

Binding an NVMe controller (`radix-nvme`, ADR 0115) maps its registers
(BAR 0), disables it, gives it an admin queue of 32 entries, enables it
with 4 KiB pages and the NVM command set, and asks what it is (Identify)
and for as many I/O queues as there are CPUs, at most 16. Each queue
pair, a *lane* of the queue below, has 64 entries and an MSI-X vector
aimed at its CPU (`allocate_interrupt_at`); the admin queue is polled.
A controller that takes shadow doorbells (Doorbell Buffer Config, OACS
bit 8), as QEMU's does, is given two pages for them before its queues
are made, and again after a reset (ADR 0117): a lane then writes each
doorbell to memory, and its register only when the controller's event
index asks.
Each active namespace with blocks of 512 to 4096 bytes and no metadata
becomes a disk, its requests naming it and counting in its blocks; its physical block is the size it is best
written in, but no more than what is whole after a power failure. At
power off, after the filesystems are written back, every disk's cache is
flushed and each NVMe controller is shut down normally (CC.SHN).

### The queue

Every disk's requests go through one queue (`fs/disk/queue.rs`, ADR
0111), and a driver is a `Controller` below it, in one *lane* or more:
each lane has its own lock, slots and waiting threads, and a thread's
requests go to its CPU's lane. An NVMe controller has a lane for each
CPU, shared by its namespaces (ADR 0115); an AHCI port and a virtio disk
have one. A request takes one of a lane's 64 slots, as many as the I/O
class and level of its thread allow, counted across the lanes (ADR
0093), and the controller starts it: virtio as a descriptor chain (the
header, the data segments, the status byte) matched to its slot by the
chain's head, AHCI as a command in the port's command list, the slot's
number its NCQ tag, NVMe as a command in the lane's submission queue,
the slot's number its command ID, its data in a PRP list of the slot's
own (up to 128 pages, or fewer when the controller says), or, when the
controller takes them and the data lie in runs of memory averaging 32
KiB or more, in a scatter gather list of those runs (ADR 0119). When the device takes indirect descriptors, as
QEMU's and most do, each virtio chain goes in a table of its own, one of
64 the driver keeps, and takes one entry of the ring; a request then
has up to 254 data segments, and 30 without them. With event indexes
(`VIRTIO_F_EVENT_IDX`) the driver rings the doorbell only when the
device asked to be told, and the device interrupts only for the first
completion after the driver has read every one before it, so that a
batch of completions raises one interrupt, as Linux's driver has it.
The interrupt handler collects what finished
and wakes the thread waiting on each finished slot (each thread waits
on its own), which frees it. Many
threads have requests in flight at once, up to the slots the controller
has.

A request the controller says goes alone (an ATA flush or TRIM, or any
command to a disk without NCQ) waits for the disk to empty, and while
it waits, requests of its rank and below that come after it wait behind
it; more urgent ones still pass, so nothing urgent waits on a flush of
idle work. A failed request ends in EIO, and the log says which it was
and what the controller said of it (an NVMe status), at most 10 of a
disk's in 5 seconds. One the controller hands back to go again, because
another command's error stopped it, a reset cleared it, or its NVMe
status does not say Do Not Retry, goes again, five times at most.

An AHCI port that an error stops turns its interrupts off and says so.
The next thread to wake on the disk takes the controller out of the
queue's lock, recovers it (ADR 0112: stop the engine, clear the error,
read the NCQ error log or reset the link, start again), and puts it
back, while the interrupt handler leaves the disk alone. A request
unanswered for 30 seconds has its thread reset the port the same way;
a disk that does not come back, or comes back as another disk, fails
every request from then on. An NVMe controller is reset the same way:
disabled, enabled again and its queue pairs made again, every request
in flight sent again; one that does not come back fails its requests.

### Raw access

A block device in `/devices` reads and writes raw (`radix_vfs::raw`):
straight to the disk, with nothing cached between, as Linux's do when
opened with `O_DIRECT`, but at any offset and length.

The whole sectors of a transfer that starts on a sector's boundary, into
or from memory on a sector's boundary, the disk's own sectors of 512
bytes or 4 KiB, go straight between the disk and
the program's memory, as Linux's `O_DIRECT` has them: the buffer lends
its pages (`Out::lend`, `In::lend`), as many as 256 parts of pages at a
time, a mebibyte from a page's start, and the device moves them
(`BlockDevice::read_frames`, `write_frames`). Only anonymous memory is
lent, the memory C programs have; a frame's or a ring's is copied.
`SpaceObject::lend` makes the pages as faults would (writable, for a
read), pins each in its page record (`PageInfo::pin`) under the space's
regions lock, and lets go of them when the device is done. A page the
program unmaps meanwhile stays until then. A pin is not a sharer, so the
program's own write to the page does not copy it away from the device.
A fork waits for the lendings under way: it would make the page
read-only in both processes, and the first write would copy it while the
device still writes the original.

The rest goes through pages of the kernel's, up to 16 pages a request: a
transfer off a sector's boundary, a part of a sector at either end, a
device that cannot reach memory it is lent (the RAM disk). A write that
covers part of a sector first reads the sector (the first and last of
the request), under the node's lock, so that two such writes to one
sector cannot lose each other's bytes. Reads end at the device's end; a
write at or past it fails with ENOSPC, as on Linux. An error after some
bytes moved ends the transfer short. `fsync` empties the disk's write
cache.

A raw transfer's requests take as many parts of pages as the device
does, not the 16 the page cache's take: 254 on a virtio disk, so a
mebibyte is two requests, as in Linux, 32 on an AHCI disk, and 128 on
most NVMe disks. A transfer of more
goes as several (`Disk::request_all`), up to 16 in flight at once, and
each is settled, retried or failed, before the call returns, even after
another failed, since the device may still be reaching the memory. The
device hears of them together: virtio-blk rings its doorbell once, after
the last, which lets QEMU merge requests for adjacent sectors into one
read or write of its file. The thread waits for the last request first,
so the earlier ones' completions, which usually come before it, wake no
one (ADR 0116).

`ioctl` on a block device takes Linux's requests (radix-block(2)): the
size in bytes and in sectors, the logical and physical sector sizes, the
minimum and optimal I/O sizes, the alignment offset, whether it is
read-only and whether it spins, whether discarded sectors read as zeros
(never promised), and, with the write right, BLKDISCARD and BLKFLSBUF.

## FAT

`radix-fat` implements FAT12, FAT16 and FAT32 from Microsoft's
specification (ADR 0056). `bpb` reads the boot sector and works out the
geometry; `table` packs and unpacks entries of each width; `name` holds
short entries, long-name entries and their checksums, and makes short
names for long ones (`NAME~1.TXT`) as Windows does; `time` converts
between FAT's local date and time fields and nanoseconds since 1970 with
Howard Hinnant's civil-date algorithms.

- A volume's metadata (both tables, directories, FSInfo) goes through its
  block cache. A file's data goes through its page cache, whose backing
  maps each page to the clusters of the file's chain and moves it
  straight between the page and the disk.
- A node knows where its short entry is, its first cluster and, once it
  is used, its whole chain. A file's size lives in its cache while it is
  in memory and reaches the entry, with its first cluster and its time,
  when the node is flushed; a node with changes is kept on the volume's
  dirty list until then, so nothing is lost when its last handle goes.
- The node table maps an entry's place to the node in memory for it, so
  two opens of one file share one node and one cache.
- A file or directory removed while in use loses its entry at once and
  keeps its clusters until its last reference goes; then they are freed.
- Freeing a directory's clusters discards them from the block cache, so
  entries written before the free are never written later over whatever
  gets the clusters next. A file's clusters need nothing like that: its
  dirty pages past a cut are dropped before the cut, and a removed file's
  pages go with it.
- Both tables are written; FSInfo's free count and hint are kept, and
  counted once from the table when FSInfo has none.

## Btrfs

`radix-btrfs` reads Btrfs from its published format documentation (ADRs
0125, 0127 and 0128): Phase 10b.1 has its trees, 10b.2 its files and its
mount, 10b.3 its compressed extents, read-only until 10c writes it.

- `csum` has the four checksums, each the crate's own code: CRC-32C by
  slicing eight bytes at a time, xxHash64, SHA-256 (`sha256`) and
  BLAKE2b-256 (`blake2b`). `format` has keys, item and tree numbers and
  root items, and the name hash (CRC-32C from `!1`, without the final
  inversion) that orders a directory's `DIR_ITEM`s.
- `superblock` reads the first superblock and refuses one that is
  damaged or that uses a feature Radix does not read.
- `chunk` maps logical addresses to their copies on the device: the
  chunks of the superblock's system chunk array first, then those of the
  chunk tree, which must agree with them.
- `tree` reads tree blocks and checks each before use, as Linux's tree
  checker does, against the address, level, transaction and keys its
  parent gives; a block that fails is read from the next copy. Checked
  blocks stay in a cache of 8 MiB, by address, dropped by a clock. A
  `Cursor` holds a block at each level of a tree: it seeks the first item
  at a key or after it, and advances through the leaves in order.
- `volume::Volume` is an opened device: its superblock, its chunk map, a
  block cache of its sectors, and the roots of its trees, the chunk tree
  and the root tree from the superblock, every other tree from its root
  item, and each subvolume's tree with its top directory.
- `inode`, `dir` and `extent` read the items a subvolume's tree keeps
  under each inode number: its inode item, its directory entries by name
  hash (`DIR_ITEM`) and in the order they were made (`DIR_INDEX`), and
  its file extents (inline, on the disk, or reserved), each with the
  checksum tree's checksum of its sectors.
- `fs::BtrfsFs` mounts the default subvolume, as Linux does without
  `subvol=`: the one the root tree's `default` entry names. Its nodes
  are found in a table by subvolume and inode number, of weak
  references, so a file with two names, or a subvolume found twice, is
  one node. A node is read before the table is locked, and a node read
  twice by two lookups at once is dropped, which only frees memory. A
  directory entry that names a subvolume leads to that subvolume's top
  directory; the inode numbers of the top subvolume are its files' IDs,
  and other subvolumes' carry their number from bit 40.
- `node::BtrfsNode` is a file, directory or link. A file's page cache
  reads each page from its extent: a hole or a reserved extent reads as
  zeros, an inline one from its item, and one on the disk straight into
  the page, whose checksum must then match, or the next copy is read;
  when none matches, or the sector has no checksum and the file is not
  `NODATASUM`, the read fails with `Io`, as Linux's does. A compressed
  extent is read whole, each sector checked so, uncompressed, and the
  page copied out; its other pages are offered to the page cache
  (`Backing::read_pages`), which keeps those it lacks, so an extent is
  uncompressed once, as on Linux.
- `inflate` (zlib, RFC 1950 and 1951), `zstd` (RFC 8878, with its XXH64
  checksum) and `lzo` (LZO1X in Btrfs's segments) are the crate's own
  decoders, written from the formats' descriptions (ADR 0128): safe,
  bounds-checked, with every table allocated once, so data that does not
  decode fails with `Io`. `unpack::Workspace` holds an extent's
  compressed bytes, its data and the decoders' tables, about 400 KiB, one
  per filesystem, made at its first compressed read and dropped by its
  shrinker.

## tmpfs and the generated trees

tmpfs is a page cache with nothing below it: its pages are the only copy,
nothing is written back, and a tmpfs has a size limit (half of memory,
as on Linux, for those programs mount; 1 MiB for the root, which holds
only mount points). Like Linux's `nr_inodes`, a tmpfs also holds at
most one file or directory for every two pages of memory, counted until
the node is dropped, so that empty files cannot use up the kernel's heap.
Directories are sorted vectors of entries, under a
mutex each, and a namespace lock per tmpfs keeps creates, removes and
renames apart.

The generated trees are made of `synthetic` nodes: fixed directories,
listed directories (whose entries come from a callback, such as the
processes) and generated files, whose text is written when the file is
opened, so that a reader sees one snapshot. They are read-only.

- `devfs`: `null`, `zero`, `random`, `urandom`, and a block device node per
  disk and partition, read and written through the block layer.
- `procfs` (radix-proc(5)): `/processes/<n>/status` for every process
  the kernel holds, `/processes/mounts`, `/processes/meminfo` and
  `/processes/uptime`.
- `sysfs` (radix-sysfs(5)): `/kernel/{version,cmdline,cpus}`,
  `/kernel/block/<name>/{size,ro}` and
  `/kernel/bus/pci/devices/<address>/{vendor,device,class}`, as Linux's
  `/sys` has them.
- `bootfs`: the boot archive's `system` directory, read-only, one node per
  file over the archive's bytes.

## Writeback and reclaim

The writeback thread wakes every five seconds and syncs every filesystem
with changes (`Vfs::sync_all`), so a power cut loses at most that much,
as with Linux's flusher threads. When the frame allocator's free pages
fall below 1/64 of memory at a wake-up, it also drops clean cached pages,
and the out-of-memory policy asks it to (`writeback::reclaim`) before it
stops a resource domain (ADR 0029). Dropping pages takes the caches'
locks, so it only ever happens in this thread, which holds none, and
never inside an allocation.

## The hierarchy

The kernel makes only the root, an empty tmpfs, and gives it to Spark in
`SLOT_ROOT`. Spark (spark(8), since v1) makes `/system`, `/applications`,
`/users`, `/shared`, `/tmp`, `/devices`, `/processes`, `/kernel` and
`/volumes`; mounts the boot archive read-only on `/system`, tmpfs on
`/system/settings`, `/system/state`, `/system/live` and `/tmp`, and the
`dev`, `proc` and `sys` trees on `/devices`, `/processes` and `/kernel`;
makes the links with Unix's names (`/dev`, `/proc`, `/sys`, `/run`,
`/etc`, `/var`, `/bin`, `/sbin`, `/usr`) and the `.hidden` file that
keeps them out of listings (ADR 0113); and mounts the FAT filesystem of
each disk or partition in `/devices` at `/volumes/<name>`, read-only
when the device is, or else its Btrfs filesystem, read-only. A disk with
a partition table is not mounted whole.
It then starts the program its arguments name with the root, read and
write, or since Phase 9 the services.

## Lock order

From outermost to innermost:

1. the capability store (released objects are queued, never dropped
   under it);
2. a FAT volume's `ns`, or a tmpfs's namespace lock;
3. a directory's lock in tmpfs, and the lock of a directory under it
   (subclass 1, only with the namespace lock held, so only one thread
   ever holds two);
4. a file's write lock, held by a write or a change of size for the
   whole call;
5. an address space's regions, held by a page fault, which fills a page
   of a mapped file by reading it;
6. a file's page cache, under which a copy to or from a program's memory
   never waits for a page: it stops, the lock is let go while the pages
   are made, and it goes on (ADR 0082);
7. a FAT node's `meta` (one at a time);
8. a FAT volume's `space`;
9. the block cache;
10. a lane of the disk's queue (with interrupts off), one at a time.

A block device node's lock for raw writes of parts of sectors is taken
with nothing else held, and its holder takes only the disk's queue. An
NVMe controller's own lock is taken by its setup, its shutdown and its
reset, with no lane's lock held (a reset has every lane's part out of
its lock); the disk's count of logged failures is taken alone. A
raw transfer lends a program's pages under its address space's regions
lock, with nothing else held, and lets go of the lock before the disk
works; a fork holds it while it waits for such transfers to end.

The mount table's lock is taken alone, around lookups and changes to the
table, never while a filesystem works. The lock checker checks this
order on every boot test (the `fuzz` test calls every directory and file
method at random with it on).

## Kernel stacks

A system call into a directory method goes through `user/files.rs`, the
VFS, FAT, the block cache, the block layer and the driver, on a 16 KiB
kernel stack. The release kernel is checked after every build (ADR
0058): `ob` reads the `.stack_sizes` section LLVM writes and refuses any
function whose frame is over 2 KiB but the few, listed with their
reasons, that run only at boot, in the panic handler or in boot tests.
Large arrays are built on the heap a slot at a time (the page index's
nodes), copies of tables go an entry at a time (the PCI functions), and
the lock checker's graph searches use bit sets instead of queues.

## Correctness

**Paths stay beneath their directory.** The walk's chain starts with the
starting place, and only a name lookup pushes onto it. `..` pops, and
popping the last entry fails. So every place the walk is ever at is
either the start or reached from it by lookups, each of which returns a
child of the place before (or, crossing a mount, the root of what is
mounted on that child). A leading `/` is refused before the walk. So a
path reaches only what lies beneath its directory, through mounts on the
way, and a directory capability gives exactly the subtree under it. The
walk without a chain only ever looks names up, so the same holds with
no pops at all; it hands any path with a `..` to the walk with one.

**A freed cluster is never written by its old owner.** Clusters change
hands between files and directories. A file's data is written back
through the file's chain as it is at the moment of the write-back, under
the file's cache lock; a directory's entries are in the block cache by
position, written back wherever they are. Freeing a file's clusters
happens after its dirty pages past the cut are dropped (a cut) or with
all its pages (a removed file whose last reference goes), under the same
lock, so no later write-back of that file can reach them. Freeing a
directory's clusters discards them from the block cache under the
volume's `space` lock, so no dirty sector of the directory survives the
free. The TLA+ model `Clusters.tla` checks, with two files, two clusters
and every interleaving of growth, writes, write-backs, frees and
reclaim, that nothing is ever written to a cluster but by its owner of
the moment and that every owner reads back what it last wrote; without
the discard it finds the stale directory sector written over a file.

**What is written reaches the disk.** A write marks its page dirty and
the node dirty; a dirty node stays in memory on the volume's list until
it is flushed, and a page is only dropped clean. The writeback thread,
`sync` and unmount write dirty pages through the chain, then the entries,
then the tables and FSInfo, then flush the device. The `storage` boot
test writes four volumes (FAT12, FAT16, FAT32 and a partitioned disk),
checks them with `fsck.fat` and mtools on the host, changes one from the
host, and reads everything back after a second boot.

## Known limits

- No name cache: every component of every path is looked up in its
  directory, and while anything is mounted, checked against the mount
  table, which is a list. Both are fine for Phase 7's trees and are the
  first things to measure as the trees grow.
- A file's cache lock is held across its disk I/O, so one file has one
  request in flight; FAT holds one namespace lock for all directory work.
- No read-ahead and no write clustering beyond one file's dirty pages in
  order; no `O_DIRECT` for files (block devices are always raw).
- Raw writes of parts of one sector through two different nodes (a disk
  and its partition) are not kept apart.
- FAT has no exFAT, no TRIM, and no journalling (as on every system: a
  power cut mid-write can leave lost clusters, which `fsck.fat` finds).
- Btrfs is read-only (10c writes it), on one device (10e), with 4 KiB
  sectors. Its compressed reads take one workspace in turn, where Linux
  keeps one per CPU at most, and are not yet measured against Linux's
  (10c adds the rows). Its `statfs` sizes are
  the superblock's, without Linux's view of the profiles and unallocated
  space.
  Its tree blocks are read through the sector cache, a page at a time,
  so a block of 16 KiB takes four requests and is held twice, in the
  sector cache and checked in its own.
- Disks are found once, at boot, and mounted by device name; there is no
  hot plug yet, and no labels in `/volumes`.
- The user heap of radix-rt keeps freed large blocks mapped and reuses
  them for blocks of the same size (docs/design/processes.md); it never
  gives memory back to its memory object.
