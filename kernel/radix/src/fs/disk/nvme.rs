// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! NVMe controllers (`radix-nvme`) and their namespaces, named `nvme0n1`,
//! `nvme0n2` and on after the controller's number and the namespace's ID,
//! as Linux names them.
//!
//! [`bind`] maps a controller's registers (BAR 0), takes it over, and
//! makes an I/O queue pair for each CPU, up to [`MAX_LANES`] and what the
//! controller grants: the lanes of one disk ([`Disk`]), which its
//! namespaces share, each a volume of it. Lane *k*'s completions raise an
//! MSI-X interrupt aimed at CPU *k*, whose handler collects that lane; a
//! controller without MSI-X vectors to spare is polled by the threads
//! waiting for it. A controller that takes shadow doorbells, as ones that
//! hypervisors emulate may, is given them: its lanes write a doorbell
//! register, an exit from the virtual machine, only when it asks. A
//! request that goes unanswered has the controller reset and its queues
//! made again ([`Controller::reset`]), and before the machine is powered
//! off each controller is told to shut down ([`shutdown`]).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use radix_block::{DeviceRef, Error, Health, Info, Page};
use radix_hal::{Devices, Smp};
use radix_hw::pci::{Bar, Function, MsiX};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_nvme::identify::{self, Unusable};
use radix_nvme::{Lane, Nvme, Shadow, command};
use radix_sync::{Mutex, Once};

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, Volume};
use super::{Entry, Ident, Name, Size, partitions, register};
use crate::Arch;

/// The most NVMe controllers.
const MAX_HOSTS: usize = 8;
/// The most lanes, I/O queue pairs, of a controller: CPUs past it share
/// them. Device vectors are numbered for the whole machine, and 160 must
/// do for every controller.
pub const MAX_LANES: usize = 16;

const _: () = assert!(radix_nvme::SLOTS == queue::SLOTS);

/// A controller, for its admin commands, and its lanes as one disk.
struct Host {
    nvme: Mutex<Nvme>,
    disk: Once<Arc<Disk<NvmeLane>, Kmalloc>>,
    /// `nvme0`, for the log.
    name: Name,
}

/// The controllers, for their interrupt handlers: set once each.
static HOSTS: [Once<Arc<Host, Kmalloc>>; MAX_HOSTS] = [const { Once::new() }; MAX_HOSTS];

/// Each controller's shadow doorbells, if it takes them: kept even if its
/// setup fails, for the next controller with its number.
static SHADOWS: [Once<Shadow>; MAX_HOSTS] = [const { Once::new() }; MAX_HOSTS];

/// Lane `arg % MAX_LANES` of controller `arg / MAX_LANES` has news.
fn on_interrupt(arg: usize) {
    let host = HOSTS.get(arg / MAX_LANES).and_then(Once::get);
    if let Some(disk) = host.and_then(|host| host.disk.get()) {
        disk.collect(arg % MAX_LANES);
    }
}

/// A lane of a controller as the queue drives it.
pub struct NvmeLane {
    lane: Lane,
    host: Arc<Host, Kmalloc>,
}

impl Controller for NvmeLane {
    fn room(&self, _op: Op, _segments: usize) -> bool {
        self.lane.room()
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, more: bool) -> Result<(), Error> {
        let op = match request.op {
            Op::Read => radix_nvme::Op::Read,
            Op::Write => radix_nvme::Op::Write,
            Op::WriteThrough => radix_nvme::Op::WriteThrough,
            Op::Flush => radix_nvme::Op::Flush,
            Op::Discard(count) => radix_nvme::Op::Deallocate(count / request.unit),
            Op::Smart(_) => return Err(Error::Unsupported),
        };
        // The namespace counts in its own blocks, the request in sectors.
        let Request { target, sector, data, unit, .. } = *request;
        let lba_bytes = (unit * radix_block::SECTOR) as u32;
        let block = sector / unit;
        self.lane.submit(slot, op, target, lba_bytes, block, data, !more).map_err(|err| match err {
            radix_nvme::Error::BadRequest => Error::OutOfRange,
            _ => Error::Io,
        })
    }

    fn kick(&mut self) {
        self.lane.ring();
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        let (slot, outcome) = self.lane.complete()?;
        let outcome = match outcome {
            radix_nvme::Outcome::Done => Outcome::Done,
            radix_nvme::Outcome::Failed => Outcome::Failed,
            radix_nvme::Outcome::Retry => Outcome::Retry,
        };
        Some((slot, outcome))
    }

    fn why(&self, slot: u8) -> Option<&'static str> {
        if self.lane.dead() {
            return Some("the controller is gone");
        }
        let status = self.lane.status(slot);
        (!status.worked()).then(|| status.describe())
    }

    fn reset(lanes: &mut [Part<NvmeLane>]) -> bool {
        let Some(host) = lanes.first().map(|lane| lane.host.clone()) else { return false };
        let mut nvme = host.nvme.lock();
        for lane in lanes.iter_mut() {
            lane.lane.restart();
        }
        let delay = &mut queue::delay;
        let result = nvme.restart(delay).and_then(|()| {
            lanes.iter_mut().try_for_each(|lane| nvme.remake(&mut lane.lane, delay))
        });
        match result {
            Ok(()) => warn!(
                "Disk: {}: a request went unanswered for 30 s; the controller was reset, and \
                 what was in flight goes again",
                host.name
            ),
            Err(err) => {
                for lane in lanes.iter_mut() {
                    lane.lane.kill();
                }
                warn!(
                    "Disk: {}: a request went unanswered for 30 s, and the controller did not \
                     come back from a reset ({err}): its disks are gone, and their requests fail",
                    host.name
                );
            }
        }
        true
    }

    fn health(disk: &Disk<NvmeLane>, _target: u32, page: &mut Page) -> Result<Health, Error> {
        let host = disk.controller(|lane| lane.host.clone()).ok_or(Error::Io)?;
        let mut log = [0u8; command::LOG_HEALTH_SIZE];
        if let Err(err) = host.nvme.lock().health(&mut queue::delay, &mut log) {
            warn!("Disk: {}: its health log could not be read: {err}", host.name);
            return Err(Error::Io);
        }
        page.bytes_mut()[..log.len()].copy_from_slice(&log);
        Ok(Health::Nvme)
    }
}

/// Sets up the NVMe controller `function` and its namespaces, as
/// controller number `next`, which moves past it if it is used; logs why
/// not if it is not. Kept out of the frame of `init`'s scan.
#[inline(never)]
pub fn bind(function: &Function, next: &mut usize) {
    if let Err(why) = try_bind(function, next) {
        warn!("Disk: could not set up the NVMe controller at {}: {why}", function.address());
    }
}

/// Why a controller is not used.
enum Why {
    Text(&'static str),
    Nvme(radix_nvme::Error),
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Text(text) => f.write_str(text),
            Why::Nvme(err) => write!(f, "{err}"),
        }
    }
}

impl From<radix_nvme::Error> for Why {
    fn from(err: radix_nvme::Error) -> Why {
        Why::Nvme(err)
    }
}

impl From<&'static str> for Why {
    fn from(text: &'static str) -> Why {
        Why::Text(text)
    }
}

fn try_bind(function: &Function, next: &mut usize) -> Result<(), Why> {
    let index = *next;
    if index >= MAX_HOSTS {
        return Err("too many controllers".into());
    }
    function.enable_memory_and_dma();
    let Some(Bar::Memory { address, size, .. }) = function.bar(radix_nvme::BAR) else {
        return Err("its registers (BAR 0) are not in memory".into());
    };
    let regs = usize::try_from(size)
        .ok()
        .and_then(|size| <Arch as Devices>::map_device(address, size))
        .ok_or("its registers could not be mapped")?;
    let source = function.address();
    let mut alloc = |pages: usize| <Arch as Devices>::alloc_dma(source, pages, false);
    let delay = &mut queue::delay;
    let mut nvme = Nvme::new(regs, &mut alloc, delay)?;
    // A lane for each CPU, within the vectors the controller has (one is
    // the admin queue's, which is polled) and the queues it grants.
    let msix = MsiX::find(function);
    let mut wanted = <Arch as Smp>::cpu_count().clamp(1, MAX_LANES);
    if let Some(msix) = msix {
        wanted = wanted.min(usize::from(msix.entries()).saturating_sub(1).max(1));
    }
    let granted = usize::from(nvme.request_queues(wanted as u16, delay)?);
    if nvme.identity().shadow_doorbells {
        let shadow = match SHADOWS[index].get() {
            Some(shadow) => Some(shadow),
            None => alloc(Shadow::PAGES)
                .and_then(Shadow::new)
                .and_then(|shadow| SHADOWS[index].set(shadow).ok()),
        };
        let given =
            shadow.ok_or(radix_nvme::Error::NoMemory).and_then(|s| nvme.use_shadow(s, delay));
        if let Err(err) = given {
            warn!(
                "Disk: nvme{index}: no shadow doorbells ({err}); every doorbell is a register write"
            );
        }
    }
    let vectors = Vectors::give(function, msix, index, granted);
    let interrupt = vectors.count > 0;
    let mut name = Name::new();
    let _ = write!(name, "nvme{index}");
    let mut lanes = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    lanes.try_reserve_exact(granted).map_err(|_| "out of memory")?;
    for lane in 0..granted {
        let vector = interrupt.then_some(lane as u16 + 1);
        let made = nvme.lane(lane as u16 + 1, vector, &mut alloc, delay)?;
        let _ = lanes.push_within_capacity(made);
    }
    let identity = *nvme.identity();
    let shadowed = nvme.shadowed();
    let (major, minor, _) = nvme.version();
    let deallocate = if identity.deallocate { u64::from(nvme.deallocate_blocks()) } else { 0 };
    let host = Host { nvme: Mutex::new(nvme), disk: Once::new(), name: name.clone() };
    let host = Arc::try_new_in(host, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let mut parts = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    parts.try_reserve_exact(granted).map_err(|_| "out of memory")?;
    for lane in lanes {
        let part = queue::part(NvmeLane { lane, host: host.clone() }).ok_or("out of memory")?;
        let _ = parts.push_within_capacity(part);
    }
    // What every namespace has; each its own size and blocks.
    let template = Info {
        sectors: 0,
        logical_block: 512,
        block_size: 512,
        read_only: false,
        max_segments: identity.max_pages,
        discard_sectors: deallocate,
        rotational: false,
    };
    let (cache, lanes) = (identity.write_cache, parts.into_iter());
    let disk = Disk::new(lanes, template, cache, true, interrupt, name.clone(), source)
        .ok_or("out of memory")?;
    let disk = Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let _ = host.disk.set(disk.clone());
    let _ = HOSTS[index].set(host.clone());
    *next += 1;
    let count = vectors.keep();
    if let Some(msix) = msix.filter(|_| interrupt) {
        msix.unmask_all(function);
    }
    let mut how = TextBuf::<96>::new();
    let _ = match count {
        0 => how.write_str("polled"),
        1 => how.write_str("one MSI-X vector"),
        n => write!(how, "an MSI-X vector for each of {n} CPUs"),
    };
    if shadowed {
        let _ = how.write_str(", shadow doorbells");
    }
    if identity.sgls {
        let _ = how.write_str(", scatter gather lists");
    }
    info!(
        "Disk: {name} is an NVMe {major}.{minor} controller at {}: {}, firmware {}, {granted} \
         I/O {} of {} commands, {how}",
        function.address(),
        identify::trimmed(&identity.model),
        identify::trimmed(&identity.firmware),
        if granted == 1 { "queue" } else { "queues" },
        radix_nvme::DEPTH,
    );
    // The namespaces are listed with the controller's lock held, and
    // added without it: reading a partition table may time out, and the
    // reset that follows takes the lock.
    let mut found = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    found.try_reserve_exact(radix_nvme::MAX_NAMESPACES).map_err(|_| "out of memory")?;
    let listed = host.nvme.lock().namespaces(&mut queue::delay, &mut |id, answer| {
        let _ = found.push_within_capacity((id, answer));
    });
    if let Err(err) = listed {
        warn!("Disk: {name}: could not list its namespaces: {err}");
    }
    for (id, answer) in found {
        add(&host, &disk, &template, &identity, id, answer);
    }
    Ok(())
}

/// The MSI-X vectors of a controller's lanes, given back unless kept:
/// when its setup fails.
struct Vectors {
    given: [u32; MAX_LANES],
    count: usize,
}

impl Vectors {
    /// Gives `lanes` lanes of controller `host` MSI-X vectors, lane *k*'s
    /// aimed at CPU *k* and in table entry *k* + 1: every lane one, or
    /// none, since a lane without would wait for its completions until a
    /// thread polled it.
    fn give(function: &Function, msix: Option<MsiX>, host: usize, lanes: usize) -> Vectors {
        let mut vectors = Vectors { given: [0; MAX_LANES], count: 0 };
        let Some(msix) = msix else { return vectors };
        let (bar, offset) = msix.table_location();
        let Some(Bar::Memory { address, .. }) = function.bar(bar) else { return vectors };
        let Some(table) = <Arch as Devices>::map_device(address + offset, msix.table_len()) else {
            return vectors;
        };
        msix.enable_masked(function);
        for lane in 0..lanes.min(MAX_LANES) {
            let arg = host * MAX_LANES + lane;
            let Some((vector, message)) = <Arch as Devices>::allocate_interrupt_at(
                function.address(),
                lane,
                on_interrupt,
                arg,
            ) else {
                msix.disable(function);
                drop(vectors);
                return Vectors { given: [0; MAX_LANES], count: 0 };
            };
            msix.set_entry(&table, lane as u16 + 1, message);
            vectors.given[lane] = vector;
            vectors.count += 1;
        }
        vectors
    }

    /// Keeps the vectors: the controller is set up. Returns how many.
    fn keep(mut self) -> usize {
        core::mem::replace(&mut self.count, 0)
    }
}

impl Drop for Vectors {
    fn drop(&mut self) {
        for &vector in &self.given[..self.count] {
            <Arch as Devices>::free_interrupt(vector);
        }
    }
}

/// Adds namespace `id` of `host`, or says why not.
fn add(
    host: &Host,
    disk: &Arc<Disk<NvmeLane>, Kmalloc>,
    template: &Info,
    identity: &identify::Controller,
    id: u32,
    answer: Result<radix_nvme::Namespace, Unusable>,
) {
    let mut name = Name::new();
    let _ = write!(name, "{}n{id}", host.name);
    let namespace = match answer {
        Ok(namespace) => namespace.info,
        Err(why) => {
            let mut text = TextBuf::<64>::new();
            let _ = match why {
                Unusable::BlockSize(0) => text.write_str("its block size is not one it lists"),
                Unusable::BlockSize(size) => {
                    write!(text, "its blocks are {size} bytes, and only 512 to 4096 are supported")
                }
                Unusable::Metadata => text.write_str("its blocks carry metadata"),
                Unusable::Empty => text.write_str("it has no blocks"),
            };
            info!("Disk: {name}: {text}; not used");
            return;
        }
    };
    // Its blocks, and the most a deallocation covers, in sectors.
    let unit = u64::from(namespace.lba_size) / radix_block::SECTOR;
    let Some(sectors) = namespace.blocks.checked_mul(unit) else {
        info!("Disk: {name}: it has more blocks than can be counted; not used");
        return;
    };
    let info = Info {
        sectors,
        logical_block: namespace.lba_size,
        block_size: namespace.block_size,
        read_only: namespace.read_only,
        discard_sectors: template.discard_sectors.saturating_mul(unit),
        ..*template
    };
    let volume = Volume::new(disk.clone(), id, info);
    let Some(Ok(volume)) = volume.map(|v| Arc::try_new_in(v, HEAP.with(Flags::MAY_SLEEP))) else {
        warn!("Disk: out of memory adding {name}");
        return;
    };
    info!(
        "Disk: {name} is an NVMe namespace of {}: {}, {} bytes a block{}{}{}{}",
        Size(info.sectors * radix_block::SECTOR),
        identify::trimmed(&identity.model),
        info.block_size,
        super::sector_note(&info),
        if info.read_only { ", read-only" } else { "" },
        if identity.write_cache { ", write cache" } else { "" },
        if info.discard_sectors > 0 { ", deallocates" } else { "" },
    );
    let device: DeviceRef = volume;
    let ident = Ident {
        model: identity.model,
        serial: super::padded(&identity.serial),
        firmware: identity.firmware,
    };
    register(Entry { name: name.clone(), device: device.clone(), place: None, ident: Some(ident) });
    partitions(&name, &device);
}

/// Tells every controller to shut down, once its namespaces' caches are
/// flushed: the machine is about to be powered off or restarted.
pub fn shutdown() {
    for host in HOSTS.iter().filter_map(Once::get) {
        if !host.nvme.lock().shutdown(&mut queue::delay) {
            warn!("Disk: {}: the controller did not finish shutting down", host.name);
        }
    }
}
