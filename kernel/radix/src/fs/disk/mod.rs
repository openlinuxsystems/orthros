// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Disks: PCI discovery, and the drivers of the disks found.
//!
//! [`init`] walks every PCI segment the firmware describes (`radix_hw::pci`),
//! logs each function it finds, and binds the disks' controllers: virtio
//! block devices ([`virtio`], named `vda`, `vdb` and on), AHCI controllers
//! ([`ahci`], whose disks are named `sda`, `sdb` and on) and NVMe
//! controllers ([`nvme`], whose namespaces are named `nvme0n1` and on). A
//! disk's partitions are named after it (`vda1`, `sda2`, and with a `p`
//! after a name that ends in a digit, `nvme0n1p1`), read from its MBR or
//! GPT (`radix_block::partition`); every one appears in `/devices` and
//! `/kernel/block`. Requests to every disk go through the same queue
//! ([`queue`]), whatever drives it. Before the machine is powered off,
//! [`shutdown`] empties every disk's write cache and shuts the NVMe
//! controllers down.

use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::AtomicU64;

use radix_abi::Status;
use radix_abi::block as request;
use radix_abi::errors::{File as FileError, Kernel};
use radix_block::{DeviceRef, Info, partition};
use radix_hal::{Devices, PciSegment, UserMode};
use radix_hw::pci::{self, Function};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::{Mutex, SpinLock};
use radix_virtio::device_type;

use crate::Arch;

mod ahci;
mod nvme;
mod queue;
mod virtio;

/// A disk's name: `vda`, `vda1`.
pub type Name = TextBuf<16>;

/// What every disk has done since boot, for `/processes/stat` and vmstat(8):
/// the reads and writes finished, and the 512-byte sectors they moved.
pub static READS: AtomicU64 = AtomicU64::new(0);
pub static READ_SECTORS: AtomicU64 = AtomicU64::new(0);
pub static WRITES: AtomicU64 = AtomicU64::new(0);
pub static WRITTEN_SECTORS: AtomicU64 = AtomicU64::new(0);

/// What the PCI scan found: where each function is, its vendor and
/// device IDs, its class code, revision and subsystem, for
/// `/kernel/bus/pci`.
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
    /// Its revision.
    pub revision: u8,
    /// Its subsystem vendor and subsystem IDs, (0, 0) for none.
    pub subsystem: (u16, u16),
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

/// Where a partition is on its disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// Its number in the disk's table, as in its name.
    pub number: u32,
    /// Its first sector on the disk.
    pub first: u64,
}

/// What a disk says it is, as its controller identifies it: text padded
/// with spaces or NULs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ident {
    /// Its model.
    pub model: [u8; 40],
    /// Its serial number.
    pub serial: [u8; 20],
    /// Its firmware's revision.
    pub firmware: [u8; 8],
}

impl Ident {
    /// Its model, without the padding.
    pub fn model(&self) -> &str {
        unpadded(&self.model)
    }

    /// Its serial number, without the padding.
    pub fn serial(&self) -> &str {
        unpadded(&self.serial)
    }

    /// Its firmware's revision, without the padding.
    pub fn firmware(&self) -> &str {
        unpadded(&self.firmware)
    }
}

/// Text a disk gave, without the spaces and NULs around it; empty if it
/// is not UTF-8.
fn unpadded(text: &[u8]) -> &str {
    core::str::from_utf8(text).unwrap_or("").trim_matches(|c: char| c == ' ' || c == '\0')
}

/// A disk or partition the kernel has.
pub struct Entry {
    /// Its name, as in `/devices`.
    pub name: Name,
    pub device: DeviceRef,
    /// For a partition, where it is on its disk.
    pub place: Option<Place>,
    /// For a disk that says, what it is.
    pub ident: Option<Ident>,
}

/// Every disk and partition, in the order they were found.
static DISKS: Mutex<Option<alloc::vec::Vec<Entry, Kmalloc>>> = Mutex::new(None);

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
    let mut sata = 0;
    let mut controllers = 0;
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
                match virtio::bind(&function, disks) {
                    Ok(()) => disks += 1,
                    Err(why) => warn!("Disk: could not set up a virtio block device: {why}"),
                }
            } else if function.class() == radix_ahci::PCI_CLASS {
                if let Err(why) = ahci::bind(&function, &mut sata) {
                    warn!(
                        "Disk: could not set up the AHCI controller at {}: {why}",
                        function.address()
                    );
                }
            } else if function.class() == radix_nvme::PCI_CLASS {
                nvme::bind(&function, &mut controllers);
            }
        });
    }
    let plural = |n: usize, one: &'static str, many: &'static str| if n == 1 { one } else { many };
    info!(
        "PCI: {functions} {} on {count} {}; {disks} virtio {}, {sata} SATA {}, {controllers} NVMe {}",
        plural(functions, "function", "functions"),
        plural(count, "segment", "segments"),
        plural(disks, "disk", "disks"),
        plural(sata, "disk", "disks"),
        plural(controllers, "controller", "controllers")
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
                revision: function.revision(),
                subsystem: function.subsystem(),
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
        (0xff | 0x00, _) => "unclassified device",
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

/// What a disk's line in the log says of its sectors: nothing for those
/// of 512 bytes, else their size (a disk with 4 KiB sectors).
fn sector_note(info: &Info) -> TextBuf<24> {
    let mut note = TextBuf::new();
    if info.logical_block != radix_block::SECTOR as u32 {
        let _ = write!(note, ", {}-byte sectors", info.logical_block);
    }
    note
}

/// Reads the partition table of the disk `name` and registers each
/// partition.
fn partitions(name: &Name, device: &DeviceRef) {
    let info = device.info();
    let mut read = |sector: u64, out: &mut [u8; 512]| {
        radix_block::read_sector(&**device, sector, out).map_err(|_| partition::Error::Io)
    };
    let (scheme, found) = match partition::read(info.sectors, info.unit(), &mut read) {
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
        partition::Scheme::GptBackup => {
            warn!(
                "Disk: {name}: its GPT's primary header or entries are damaged; using the backup"
            );
            "a GPT"
        }
    };
    let count = found.len();
    info!(
        "Disk: {name} has {scheme} with {count} {}",
        if count == 1 { "partition" } else { "partitions" }
    );
    for part in &found {
        let Some(slice) = radix_block::Slice::new(device.clone(), part.first, part.sectors) else {
            continue;
        };
        let Ok(slice) = radix_vfs::mem::arc(slice) else { continue };
        let mut part_name = Name::new();
        let digit = name.as_str().ends_with(|c: char| c.is_ascii_digit());
        let _ = write!(part_name, "{name}{}{}", if digit { "p" } else { "" }, part.number);
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
            "Disk: {part_name}: {} from sector {}, {kind}{}",
            Size(part.sectors * radix_block::SECTOR),
            part.first,
            if part.truncated {
                ", cut to the disk's end (its table has it reach past)"
            } else {
                ""
            }
        );
        let device: DeviceRef = slice;
        let place = Place { number: part.number, first: part.first };
        register(Entry { name: part_name, device, place: Some(place), ident: None });
    }
}

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

/// Adds a disk or partition under `name`, to `/devices` and `/kernel/block`.
fn register(entry: Entry) {
    let (name, device) = (entry.name.clone(), entry.device.clone());
    {
        let mut disks = DISKS.lock();
        let disks =
            disks.get_or_insert_with(|| alloc::vec::Vec::new_in(HEAP.with(Flags::MAY_SLEEP)));
        if disks.try_reserve(1).is_err() {
            warn!("Disk: out of memory registering {name}");
            return;
        }
        let _ = disks.push_within_capacity(entry);
    }
    super::devfs::add_block(name.as_str(), device);
}

/// Readies every disk for the power to go: empties each one's write
/// cache, and tells each NVMe controller to shut down. Runs once the
/// filesystems have written back what they hold, before the machine is
/// powered off, restarted or halted; requests after it may fail.
pub fn shutdown() {
    for_each(|entry| {
        if let Err(err) = entry.device.flush() {
            warn!("Disk: {}: its write cache could not be emptied: {err}", entry.name);
        }
    });
    nvme::shutdown();
}

/// The name a disk or partition was registered under.
pub fn name_of(device: &DeviceRef) -> Option<Name> {
    let disks = DISKS.lock();
    disks.as_ref()?.iter().find(|e| Arc::ptr_eq(&e.device, device)).map(|e| e.name.clone())
}

/// Calls `f` with each disk and partition, in the order they were found.
pub fn for_each(mut f: impl FnMut(&Entry)) {
    let disks = DISKS.lock();
    for entry in disks.iter().flatten() {
        f(entry);
    }
}

/// radix-block(2)'s `control` requests on `device`: `argument` is the
/// caller's address of the value a request writes or reads, and
/// `may_write` whether the caller's capability has the write right, which
/// DISCARD and FLUSH need. Any other request is not one of a block device.
pub fn control(
    device: &DeviceRef,
    request: u32,
    argument: u64,
    may_write: bool,
) -> Result<(), Status> {
    let info = device.info();
    let put = |bytes: &[u8]| {
        if <Arch as UserMode>::copy_to_user(argument, bytes) {
            Ok(())
        } else {
            Err(Kernel::BadAddress.status(0))
        }
    };
    match request {
        request::GET_SIZE_BYTES => put(&(info.sectors * radix_block::SECTOR).to_le_bytes()),
        request::GET_SIZE_SECTORS => put(&info.sectors.to_le_bytes()),
        request::GET_SECTOR_SIZE => put(&info.logical_block.to_le_bytes()),
        request::GET_PHYSICAL_BLOCK_SIZE | request::GET_MINIMUM_IO => {
            put(&info.block_size.to_le_bytes())
        }
        request::GET_OPTIMAL_IO | request::GET_ALIGNMENT_OFFSET | request::GET_DISCARD_ZEROES => {
            put(&0u32.to_le_bytes())
        }
        request::GET_READ_ONLY => put(&u32::from(info.read_only).to_le_bytes()),
        request::GET_ROTATIONAL => put(&u16::from(info.rotational).to_le_bytes()),
        request::DISCARD | request::FLUSH if !may_write => Err(Kernel::MissingRights.status(0)),
        request::DISCARD => {
            let mut range = [0u8; 16];
            if !<Arch as UserMode>::copy_from_user(&mut range, argument) {
                return Err(Kernel::BadAddress.status(0));
            }
            let (pair, _) = range.as_chunks::<8>();
            let (start, len) = (u64::from_le_bytes(pair[0]), u64::from_le_bytes(pair[1]));
            // Whole logical blocks, as Linux takes them.
            let (sector, block) = (radix_block::SECTOR, u64::from(info.logical_block));
            let fits = start.checked_add(len).is_some_and(|end| end <= info.sectors * sector);
            if !start.is_multiple_of(block) || !len.is_multiple_of(block) || !fits {
                return Err(Kernel::InvalidArgument.status(1));
            }
            if len == 0 {
                return Ok(());
            }
            device.discard(start / sector, len / sector).map_err(block_status)
        }
        request::FLUSH => device.flush().map_err(block_status),
        _ => Err(Kernel::NotTerminal.status(0)),
    }
}

/// The status of a block layer error.
fn block_status(err: radix_block::Error) -> Status {
    match err {
        radix_block::Error::Unsupported => Kernel::NotSupported.status(0),
        radix_block::Error::OutOfRange => Kernel::InvalidArgument.status(1),
        radix_block::Error::NoMemory => Kernel::NoMemory.status(0),
        radix_block::Error::ReadOnly => FileError::ReadOnly.status(0),
        radix_block::Error::Io => FileError::Io.status(0),
    }
}
