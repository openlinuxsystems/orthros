// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Disks: PCI discovery and virtio block devices.
//!
//! [`init`] walks every PCI segment the firmware describes (`radix_hw::pci`),
//! logs each function it finds, and binds the virtio block devices
//! (`radix-virtio`): it maps their registers, turns on bus mastering, gives
//! each one MSI-X interrupt for its queue, and sets the device up. Each
//! disk is named as Linux names them, `vda`, `vdb` and on, and so are its
//! partitions (`vda1`), read from its MBR or GPT (`radix_block::partition`);
//! every one appears in `/dev` and `/sys/block`.
//!
//! A request blocks its thread until the device is done; many threads have
//! requests in flight at once, up to what the queue holds. A request takes
//! one of [`REQUESTS`] slots, whose state the interrupt handler sets when
//! the device hands its descriptor chain back; the thread then frees the
//! slot. The chain's head may be reused as soon as it is handed back, so
//! completions are matched to slots by head at that moment, under the
//! queue's lock. A device with no MSI-X vector to spare is polled by the
//! threads waiting for it.
//!
//! Requests take room by their processes' I/O class and level
//! (radix-process(2)'s `io_priority`, ionice(1)): one takes room only
//! while no more urgent one waits for it, the real-time class before
//! best effort, and level 0 before 7 in each; the idle class takes room
//! only while the disk has nothing else to do. Once in flight, every
//! request is the device's alike.

use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use radix_abi::process::{IO_LEVELS, IoClass};
use radix_block::{BlockDevice, DeviceRef, Error, Info, Segment, SegmentMut, partition};
use radix_hal::{Devices, PciSegment};
use radix_hw::pci::{self, Bar, Function, MsiX};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::{Mutex, Once, SpinLock};
use radix_virtio::blk::{Blk, Op, Outcome};
use radix_virtio::{NO_VECTOR, Transport, device_type};

use crate::Arch;
use crate::sched::{self, wait};
use crate::user::posix::priority;

/// The most virtio disks.
const MAX_DISKS: usize = 16;
/// Requests in flight or waiting to be collected, per disk.
const REQUESTS: usize = 64;
/// The most segments one request takes: a page cache's requests take one
/// page each, so this is plenty.
const MAX_SEGMENTS: usize = 16;

const FREE: u8 = 0;
const PENDING: u8 = 1;
const DONE: u8 = 2;
const FAILED: u8 = 3;

/// How many ranks of urgency requests have (`posix::priority::io_rank`):
/// the real-time class's levels, the best-effort class's, and idle.
const RANKS: usize = 2 * IO_LEVELS as usize + 1;
/// The idle class's rank, the least urgent.
const IDLE: usize = RANKS - 1;

/// A disk's name: `vda`, `vda1`.
pub type Name = TextBuf<16>;

/// What every disk has done since boot, for `/proc/stat` and vmstat(8):
/// the reads and writes finished, and the 512-byte sectors they moved.
pub static READS: AtomicU64 = AtomicU64::new(0);
pub static READ_SECTORS: AtomicU64 = AtomicU64::new(0);
pub static WRITES: AtomicU64 = AtomicU64::new(0);
pub static WRITTEN_SECTORS: AtomicU64 = AtomicU64::new(0);

/// Takes a waiter off [`sched::DISK_WAITERS`] when it goes.
struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        sched::DISK_WAITERS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Takes a request off its disk's count of those waiting for room at its
/// rank when it goes.
struct Waiting<'a>(&'a AtomicU32);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How urgently the running thread's requests are served: by its
/// process's I/O class and level (radix-process(2)'s `io_priority`), or
/// for the kernel's own threads, the middle of best effort.
fn running_rank() -> usize {
    let class = sched::running()
        .and_then(|me| me.user().map(|user| user.process.io_priority()))
        .unwrap_or((IoClass::None.raw(), 0));
    (priority::io_rank(class.0, class.1) as usize).min(IDLE)
}

/// A virtio block device.
pub struct VirtioDisk {
    index: usize,
    queue: SpinLock<Queue>,
    /// Each request slot's state.
    requests: [AtomicU8; REQUESTS],
    /// Moves on every completion and every slot freed: what threads
    /// waiting for room wait on.
    events: AtomicU64,
    /// How many requests wait for room, by rank.
    waiting: [AtomicU32; RANKS],
    info: radix_virtio::blk::Info,
    /// Its interrupt vector, if it has one; otherwise waiters poll.
    vector: Option<u32>,
}

/// The driver and which slot each descriptor chain in flight belongs to.
struct Queue {
    blk: Blk,
    owner: [u8; radix_virtio::queue::MAX_SIZE as usize],
}

/// The disks, for their interrupt handlers: set once each.
static VIRTIO: [Once<Arc<VirtioDisk, Kmalloc>>; MAX_DISKS] = [const { Once::new() }; MAX_DISKS];

/// What the PCI scan found: where each function is, its vendor and
/// device IDs, and its class code, for `/sys/bus/pci`.
#[derive(Clone, Copy, Debug)]
pub struct PciFunction {
    /// Where it is.
    pub address: pci::Address,
    /// Its vendor ID.
    pub vendor: u16,
    /// Its device ID.
    pub device: u16,
    /// Its class, subclass and programming interface.
    pub class: u32,
}

/// The most functions [`pci_functions`] lists.
const MAX_FUNCTIONS: usize = 128;

static FUNCTIONS: SpinLock<([Option<PciFunction>; MAX_FUNCTIONS], usize)> =
    SpinLock::new(([None; MAX_FUNCTIONS], 0));

/// Calls `f` with every PCI function the scan found, in order.
pub fn pci_functions(mut f: impl FnMut(&PciFunction)) {
    // One at a time, and without the lock held while `f` runs: a copy of
    // the whole table would take 2.5 KiB of a kernel stack.
    for index in 0..MAX_FUNCTIONS {
        let Some(function) = FUNCTIONS.lock().0[index] else { break };
        f(&function);
    }
}

/// Every disk and partition, by name.
static DISKS: Mutex<Option<alloc::vec::Vec<(Name, DeviceRef), Kmalloc>>> = Mutex::new(None);

fn on_interrupt(index: usize) {
    if let Some(disk) = VIRTIO.get(index).and_then(Once::get) {
        disk.collect();
    }
}

impl VirtioDisk {
    fn key(&self) -> usize {
        self as *const VirtioDisk as usize
    }

    /// Takes the completions the device has handed back, and wakes the
    /// threads waiting for them. Runs in the interrupt handler, and in
    /// waiting threads when the device has no interrupt.
    fn collect(&self) {
        let mut any = false;
        {
            let mut queue = self.queue.lock_irq();
            while let Some((head, outcome)) = queue.blk.complete() {
                let slot = usize::from(queue.owner[usize::from(head) % queue.owner.len()]);
                let state = if outcome == Outcome::Done { DONE } else { FAILED };
                if let Some(request) = self.requests.get(slot) {
                    request.store(state, Ordering::Release);
                }
                any = true;
            }
        }
        if any {
            self.events.fetch_add(1, Ordering::Release);
            wait::wake_all(self.key());
        }
    }

    /// Waits until `condition` holds: asleep until an interrupt, or
    /// polling the device without one.
    fn wait(&self, condition: impl Fn() -> bool) {
        // Counted in the load averages while it waits, as on Linux.
        sched::DISK_WAITERS.fetch_add(1, Ordering::Relaxed);
        let _counted = Counted;
        if self.vector.is_some() {
            wait::wait_until(self.key(), "disk", condition);
        } else {
            while !condition() {
                self.collect();
                if !condition() {
                    sched::yield_now();
                }
            }
        }
    }

    /// Whether a request of rank `rank` may take room now: no more urgent
    /// one waits for it, and for the idle class, no other request waits or
    /// is in flight, as Linux's idle class is served only when the disk
    /// is otherwise idle.
    fn may_go(&self, rank: usize) -> bool {
        let others = |range: core::ops::Range<usize>| {
            self.waiting[range].iter().any(|count| count.load(Ordering::Acquire) > 0)
        };
        if rank < IDLE {
            return !others(0..rank);
        }
        !others(0..IDLE) && self.requests.iter().all(|r| r.load(Ordering::Acquire) == FREE)
    }

    /// Carries out one request and waits for it.
    fn request(&self, op: Op, sector: u64, data: &[(u64, u32)]) -> Result<(), Error> {
        let rank = running_rank();
        self.waiting[rank].fetch_add(1, Ordering::AcqRel);
        let waiting = Waiting(&self.waiting[rank]);
        let slot = loop {
            let seen = self.events.load(Ordering::Acquire);
            if self.may_go(rank) {
                let mut queue = self.queue.lock_irq();
                let free = self.requests.iter().position(|r| r.load(Ordering::Acquire) == FREE);
                if let Some(slot) = free
                    && queue.blk.room(data.len()) > 0
                {
                    let head = queue.blk.submit(op, sector, data).map_err(|err| match err {
                        radix_virtio::Error::BadRequest => Error::OutOfRange,
                        _ => Error::Io,
                    })?;
                    self.requests[slot].store(PENDING, Ordering::Relaxed);
                    let len = queue.owner.len();
                    queue.owner[usize::from(head) % len] = slot as u8;
                    break slot;
                }
            }
            // No room, or a more urgent request comes first: wait for a
            // completion, or a slot freed.
            self.wait(|| self.events.load(Ordering::Acquire) != seen);
        };
        drop(waiting);
        if self.waiting.iter().any(|count| count.load(Ordering::Acquire) > 0) {
            // Less urgent requests waited for this one: there may be room
            // for them too.
            self.events.fetch_add(1, Ordering::Release);
            wait::wake_all(self.key());
        }
        self.wait(|| self.requests[slot].load(Ordering::Acquire) != PENDING);
        let state = self.requests[slot].swap(FREE, Ordering::AcqRel);
        self.events.fetch_add(1, Ordering::Release);
        wait::wake_all(self.key());
        if state != DONE {
            return Err(Error::Io);
        }
        let sectors = data.iter().map(|&(_, len)| u64::from(len)).sum::<u64>() / 512;
        let (count, moved) = match op {
            Op::Read => (&READS, &READ_SECTORS),
            Op::Write => (&WRITES, &WRITTEN_SECTORS),
            _ => return Ok(()),
        };
        count.fetch_add(1, Ordering::Relaxed);
        moved.fetch_add(sectors, Ordering::Relaxed);
        Ok(())
    }
}

impl BlockDevice for VirtioDisk {
    fn info(&self) -> Info {
        Info {
            sectors: self.info.sectors,
            block_size: self.info.block_size,
            read_only: self.info.read_only,
            max_segments: (self.info.max_segments as usize).min(MAX_SEGMENTS),
        }
    }

    fn read(&self, sector: u64, segments: &mut [SegmentMut<'_>]) -> Result<(), Error> {
        let max = self.info().max_segments;
        radix_block::check(
            self.info.sectors,
            max,
            sector,
            segments.iter().map(|s| (s.offset, s.len)),
        )?;
        let mut data = [(0u64, 0u32); MAX_SEGMENTS];
        for (i, segment) in segments.iter().enumerate() {
            data[i] = (segment.page.phys() + segment.offset as u64, segment.len as u32);
        }
        self.request(Op::Read, sector, &data[..segments.len()])?;
        for segment in segments.iter_mut() {
            let (offset, len) = (segment.offset, segment.len);
            radix_hw::device_wrote(&mut segment.page.bytes_mut()[offset..offset + len]);
        }
        Ok(())
    }

    fn write(&self, sector: u64, segments: &[Segment<'_>]) -> Result<(), Error> {
        if self.info.read_only {
            return Err(Error::ReadOnly);
        }
        let max = self.info().max_segments;
        radix_block::check(
            self.info.sectors,
            max,
            sector,
            segments.iter().map(|s| (s.offset, s.len)),
        )?;
        let mut data = [(0u64, 0u32); MAX_SEGMENTS];
        for (i, segment) in segments.iter().enumerate() {
            data[i] = (segment.page.phys() + segment.offset as u64, segment.len as u32);
        }
        radix_hw::device_barrier();
        self.request(Op::Write, sector, &data[..segments.len()])
    }

    fn flush(&self) -> Result<(), Error> {
        if !self.info.flush {
            // Without a write cache, a write that returned is durable.
            return Ok(());
        }
        self.request(Op::Flush, 0, &[])
    }
}

/// Finds and sets up the disks.
pub fn init() {
    let mut segments = [None; 8];
    let mut count = 0;
    <Arch as Devices>::pci_segments(&mut |segment| {
        if count < segments.len() {
            segments[count] = Some(segment);
            count += 1;
        }
    });
    if count == 0 {
        info!("PCI: the firmware describes no PCI Express configuration space; no disks");
        return;
    }
    let mut functions = 0;
    let mut disks = 0;
    for segment in segments.iter().flatten() {
        let PciSegment { base, segment: number, first_bus, last_bus } = *segment;
        let mut map_bus = |bus: u8| {
            let phys = base + (u64::from(bus) << 20);
            <Arch as Devices>::map_device(phys, pci::BUS_SIZE)
        };
        pci::scan(number, first_bus, last_bus, &mut map_bus, &mut |function| {
            functions += 1;
            describe(&function);
            if device_type(&function) == Some(radix_virtio::device_type::BLOCK) {
                match bind(function, disks) {
                    Ok(()) => disks += 1,
                    Err(why) => warn!("Disk: could not set up a virtio block device: {why}"),
                }
            }
        });
    }
    let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    info!(
        "PCI: {functions} {} on {count} {}; {disks} virtio {}",
        plural(functions, "function", "functions"),
        plural(count, "segment", "segments"),
        plural(disks, "disk", "disks")
    );
}

/// Logs a function: where it is, its IDs and what its class says it is.
fn describe(function: &Function) {
    let (class, subclass, interface) = function.class();
    {
        let mut found = FUNCTIONS.lock();
        let count = found.1;
        if count < MAX_FUNCTIONS {
            found.0[count] = Some(PciFunction {
                address: function.address(),
                vendor: function.vendor_id(),
                device: function.device_id(),
                class: (u32::from(class) << 16) | (u32::from(subclass) << 8) | u32::from(interface),
            });
            found.1 += 1;
        }
    }
    let what = match (class, subclass) {
        (0x01, 0x00) => "SCSI controller",
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x08) => "NVMe controller",
        (0x01, _) => "storage controller",
        (0x02, 0x00) => "Ethernet controller",
        (0x02, 0x80) => "network controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x04, 0x03) => "audio device",
        (0x04, _) => "multimedia device",
        (0x05, _) => "memory controller",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x07, _) => "communication controller",
        (0x08, _) => "system peripheral",
        (0x0c, 0x03) => "USB controller",
        (0x0c, 0x05) => "SMBus controller",
        (0x0c, _) => "serial bus controller",
        (0x0d, _) => "wireless controller",
        (0x10, _) => "encryption controller",
        (0x11, _) => "signal processing controller",
        (0xff, _) | (0x00, _) => "unclassified device",
        _ => "device",
    };
    let virtio = if device_type(function).is_some() { ", virtio" } else { "" };
    info!(
        "PCI: {} {:04x}:{:04x} class {class:02x}{subclass:02x}{interface:02x}: {what}{virtio}",
        function.address(),
        function.vendor_id(),
        function.device_id()
    );
}

/// Sets up the virtio block device `function` as disk number `index`.
fn bind(function: Function, index: usize) -> Result<(), &'static str> {
    if index >= MAX_DISKS {
        return Err("too many disks");
    }
    function.enable_memory_and_dma();
    let mut map_bar = |_: usize, bar: Bar| match bar {
        Bar::Memory { address, size, .. } => {
            <Arch as Devices>::map_device(address, usize::try_from(size).ok()?)
        }
        Bar::Io { .. } => None,
    };
    let transport = Transport::find(&function, &mut map_bar).map_err(|_| "no modern transport")?;
    // One MSI-X entry, for the queue, if the device and the kernel have
    // them; otherwise the device is polled.
    let interrupt = MsiX::find(&function).and_then(|msix| {
        let (bar, offset) = msix.table_location();
        let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return None };
        let table = <Arch as Devices>::map_device(address + offset, msix.table_len())?;
        let (vector, message) = <Arch as Devices>::allocate_interrupt(on_interrupt, index)?;
        msix.enable_masked(&function);
        msix.set_entry(&table, 0, message);
        msix.unmask_all(&function);
        Some(vector)
    });
    let entry = if interrupt.is_some() { 0 } else { NO_VECTOR };
    let mut alloc = |pages: usize| <Arch as Devices>::alloc_dma(pages, false);
    let blk = match Blk::new(transport, entry, &mut alloc) {
        Ok(blk) => blk,
        Err(_) => {
            if let Some(vector) = interrupt {
                <Arch as Devices>::free_interrupt(vector);
            }
            return Err("the device refused to be set up");
        }
    };
    let info = blk.info();
    if info.max_segment_size != 0 && (info.max_segment_size as usize) < radix_hw::PAGE_SIZE {
        blk.reset();
        return Err("its segments are smaller than a page");
    }
    let disk = VirtioDisk {
        index,
        queue: SpinLock::new(Queue { blk, owner: [0; radix_virtio::queue::MAX_SIZE as usize] }),
        requests: [const { AtomicU8::new(FREE) }; REQUESTS],
        events: AtomicU64::new(0),
        waiting: [const { AtomicU32::new(0) }; RANKS],
        info,
        vector: interrupt,
    };
    let disk = Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let _ = VIRTIO[index].set(disk.clone());
    let mut name = Name::new();
    let _ = write!(name, "vd{}", (b'a' + index as u8) as char);
    let size = Size(info.sectors * radix_block::SECTOR);
    let how = match interrupt {
        Some(vector) => {
            let mut text = TextBuf::<24>::new();
            let _ = write!(text, "MSI-X vector {vector:#x}");
            text
        }
        None => {
            let mut text = TextBuf::<24>::new();
            let _ = text.write_str("polled");
            text
        }
    };
    info!(
        "Disk: {name} is a virtio block device of {size} at {}, {} bytes a block{}{}, {how}",
        function.address(),
        info.block_size,
        if info.read_only { ", read-only" } else { "" },
        if info.flush { ", write cache" } else { "" },
    );
    let _ = disk.index;
    let device: DeviceRef = disk;
    register(&name, device.clone());
    partitions(&name, &device);
    Ok(())
}

/// Reads the partition table of the disk `name` and registers each
/// partition.
fn partitions(name: &Name, device: &DeviceRef) {
    let sectors = device.info().sectors;
    let mut read = |sector: u64, out: &mut [u8; 512]| {
        radix_block::read_sector(&**device, sector, out).map_err(|_| partition::Error::Io)
    };
    let (scheme, found) = match partition::read(sectors, &mut read) {
        Ok(read) => read,
        Err(err) => {
            warn!("Disk: {name}: {err}");
            return;
        }
    };
    let scheme = match scheme {
        partition::Scheme::None => {
            info!("Disk: {name} has no partition table; it is used whole");
            return;
        }
        partition::Scheme::Mbr => "an MBR",
        partition::Scheme::Gpt => "a GPT",
    };
    let count = found.len();
    info!(
        "Disk: {name} has {scheme} with {count} {}",
        if count == 1 { "partition" } else { "partitions" }
    );
    for part in found.iter() {
        let Some(slice) = radix_block::Slice::new(device.clone(), part.first, part.sectors) else {
            continue;
        };
        let Ok(slice) = radix_vfs::mem::arc(slice) else { continue };
        let mut part_name = Name::new();
        let _ = write!(part_name, "{name}{}", part.number);
        let mut kind = TextBuf::<48>::new();
        match part.kind {
            partition::Kind::Mbr(byte) => {
                let _ = write!(kind, "type {byte:#04x}");
            }
            partition::Kind::Gpt(_) => {
                let _ = kind.write_str("GPT");
                let mut label = part.name().peekable();
                if label.peek().is_some() {
                    let _ = kind.write_str(" \"");
                    for c in label {
                        let _ = kind.write_char(c);
                    }
                    let _ = kind.write_char('"');
                }
            }
        }
        info!(
            "Disk: {part_name}: {} from sector {}, {kind}",
            Size(part.sectors * radix_block::SECTOR),
            part.first
        );
        let device: DeviceRef = slice;
        register(&part_name, device);
    }
}

/// Adds a disk or partition under `name`, to `/dev` and `/sys/block`.
/// A size for the log, in the largest binary unit it reaches, with a
/// tenth when it is not a whole number of them: `1.4 MiB` for a floppy
/// disk, `32 MiB`.
struct Size(u64);

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (unit, shift) = [("GiB", 30), ("MiB", 20), ("KiB", 10)]
            .into_iter()
            .find(|&(_, shift)| self.0 >> shift != 0)
            .unwrap_or(("bytes", 0));
        let whole = self.0 >> shift;
        let tenths = ((self.0 - (whole << shift)) * 10) >> shift;
        match tenths {
            0 => write!(f, "{whole} {unit}"),
            _ => write!(f, "{whole}.{tenths} {unit}"),
        }
    }
}

fn register(name: &Name, device: DeviceRef) {
    {
        let mut disks = DISKS.lock();
        let disks =
            disks.get_or_insert_with(|| alloc::vec::Vec::new_in(HEAP.with(Flags::MAY_SLEEP)));
        if disks.try_reserve(1).is_err() {
            warn!("Disk: out of memory registering {name}");
            return;
        }
        let _ = disks.push_within_capacity((name.clone(), device.clone()));
    }
    super::devfs::add_block(name.as_str(), device);
}

/// The name a disk or partition was registered under.
pub fn name_of(device: &DeviceRef) -> Option<Name> {
    let disks = DISKS.lock();
    disks.as_ref()?.iter().find(|(_, d)| Arc::ptr_eq(d, device)).map(|(name, _)| name.clone())
}

/// Calls `f` with each disk and partition, in the order they were found.
pub fn for_each(mut f: impl FnMut(&str, &DeviceRef)) {
    let disks = DISKS.lock();
    for (name, device) in disks.iter().flatten() {
        f(name.as_str(), device);
    }
}
