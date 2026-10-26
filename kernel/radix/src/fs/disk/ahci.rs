// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! AHCI controllers (`radix-ahci`) and the SATA disks on them, named
//! `sda`, `sdb` and on, in the order the controllers and their ports are
//! found, as Linux names them.
//!
//! [`bind`] maps a controller's registers (BAR 5), takes it over from the
//! firmware, gives it one MSI for all its ports, and finds its disks. Each
//! disk's port drives the shared queue ([`Controller`]): a command slot
//! is matched to the queue's slot in a table of the port's. The interrupt
//! handler collects the completions of each port the controller says has
//! some, then clears the controller's status for them; a port being
//! recovered is left alone, its interrupts off until it runs again. A
//! controller without MSI is polled by the threads waiting for it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use radix_ahci::ata::trimmed;
use radix_ahci::{Absent, Hba, Port};
use radix_block::{DeviceRef, Error, Info};
use radix_hal::Devices;
use radix_hw::pci::{Bar, Function, Msi};
use radix_klog::{TextBuf, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::Once;

use super::queue::{self, Controller, Disk, Op, Outcome, Part, Request, Volume};
use super::{Name, Size, partitions, register};
use crate::Arch;

/// The most AHCI controllers.
const MAX_HOSTS: usize = 4;
/// Ports on a controller, at most.
const PORTS: usize = 32;

/// A controller and the disks on its ports.
struct Host {
    hba: Hba,
    disks: [Once<Arc<Disk<SataPort>, Kmalloc>>; PORTS],
}

/// The controllers, for their interrupt handlers: set once each.
static HOSTS: [Once<Arc<Host, Kmalloc>>; MAX_HOSTS] = [const { Once::new() }; MAX_HOSTS];

fn on_interrupt(index: usize) {
    let Some(host) = HOSTS.get(index).and_then(Once::get) else { return };
    let pending = host.hba.pending();
    if pending == 0 {
        return;
    }
    let mut left = pending;
    while left != 0 {
        let port = left.trailing_zeros() as usize;
        left &= left - 1;
        if let Some(disk) = host.disks[port].get() {
            disk.collect(0);
        }
    }
    host.hba.acknowledge(pending);
}

/// A disk's port as the queue drives it.
pub struct SataPort {
    port: Port,
    /// Which queue slot each command slot's request is.
    owner: [u8; PORTS],
    /// The disk's name, for the log.
    name: Name,
}

fn port_op(op: Op) -> radix_ahci::Op {
    match op {
        Op::Read => radix_ahci::Op::Read,
        Op::Write => radix_ahci::Op::Write,
        Op::WriteThrough => radix_ahci::Op::WriteThrough,
        Op::Flush => radix_ahci::Op::Flush,
        Op::Discard(count) => radix_ahci::Op::Trim(count),
    }
}

impl Controller for SataPort {
    fn room(&self, op: Op, _segments: usize) -> bool {
        self.port.room(port_op(op))
    }

    fn alone(&self, op: Op) -> bool {
        self.port.alone(port_op(op))
    }

    fn submit(&mut self, slot: u8, request: &Request<'_>, _more: bool) -> Result<(), Error> {
        let Request { op, sector, data, .. } = *request;
        let tag = self.port.submit(port_op(op), sector, data).map_err(|err| match err {
            radix_ahci::Error::BadRequest => Error::OutOfRange,
            _ => Error::Io,
        })?;
        self.owner[usize::from(tag) % PORTS] = slot;
        Ok(())
    }

    fn complete(&mut self) -> Option<(u8, Outcome)> {
        let (tag, outcome) = self.port.complete()?;
        let outcome = match outcome {
            radix_ahci::Outcome::Done => Outcome::Done,
            radix_ahci::Outcome::Failed => Outcome::Failed,
            radix_ahci::Outcome::Retry => Outcome::Retry,
        };
        Some((self.owner[usize::from(tag) % PORTS], outcome))
    }

    fn stopped(&self) -> bool {
        self.port.stopped()
    }

    fn recover(lanes: &mut [Part<SataPort>]) {
        for lane in lanes {
            lane.recover_port();
        }
    }

    fn reset(lanes: &mut [Part<SataPort>]) -> bool {
        for lane in lanes {
            lane.reset_link();
        }
        true
    }
}

impl SataPort {
    /// Restarts the port after an error stopped it.
    fn recover_port(&mut self) {
        if !self.port.stopped() {
            return;
        }
        let status = self.port.stop_status().unwrap_or(0);
        self.port.recover(&mut queue::delay);
        let errors = self.port.errors().0;
        if self.port.dead() {
            warn!(
                "Disk: {}: an error stopped its port (status {status:#x}), which did not \
                 restart: the disk is gone, and its requests fail",
                self.name
            );
        } else {
            warn!(
                "Disk: {}: an error stopped its port (status {status:#x}); it runs again \
                 ({errors} {} since boot)",
                self.name,
                if errors == 1 { "error" } else { "errors" }
            );
        }
    }

    /// Resets the port's link after a request went unanswered too long.
    fn reset_link(&mut self) {
        self.port.reset(&mut queue::delay);
        if self.port.dead() {
            warn!(
                "Disk: {}: a request went unanswered for 30 s, and after a link reset the \
                 disk is gone, or another one is there: its requests fail",
                self.name
            );
        } else {
            warn!(
                "Disk: {}: a request went unanswered for 30 s; the link was reset, and \
                 what was in flight goes again",
                self.name
            );
        }
    }
}

/// Sets up the AHCI controller `function` and the disks on it, the first
/// named after letter `next`, which moves past them. Kept out of the
/// frame of `init`'s scan.
#[inline(never)]
pub fn bind(function: &Function, next: &mut usize) -> Result<(), &'static str> {
    let index = HOSTS.iter().position(|host| host.get().is_none()).ok_or("too many controllers")?;
    function.enable_memory_and_dma();
    let Some(Bar::Memory { address, size, .. }) = function.bar(radix_ahci::BAR) else {
        return Err("its registers (BAR 5) are not in memory");
    };
    let regs = usize::try_from(size)
        .ok()
        .and_then(|size| <Arch as Devices>::map_device(address, size))
        .ok_or("its registers could not be mapped")?;
    let hba = Hba::new(regs, &mut queue::delay).map_err(|err| err.as_str())?;
    if !hba.wide() {
        // Its buffers would need to be in the first 4 GiB, which page
        // caches are not kept to.
        return Err("it reaches only the first 4 GiB of memory, which is not supported");
    }
    let (major, minor) = hba.version();
    let (ports, slots, ncq) = (hba.implemented().count_ones(), hba.slots(), hba.ncq());
    let host = Host { hba, disks: [const { Once::new() }; PORTS] };
    let host = Arc::try_new_in(host, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let _ = HOSTS[index].set(host.clone());
    // One MSI for every port, if the controller and the kernel have one;
    // otherwise its disks are polled.
    let interrupt = Msi::find(function).and_then(|msi| {
        let (vector, message) = <Arch as Devices>::allocate_interrupt(on_interrupt, index)?;
        msi.enable(function, message);
        Some(vector)
    });
    if interrupt.is_some() {
        host.hba.enable_interrupts();
    }
    let mut how = TextBuf::<24>::new();
    let _ = match interrupt {
        Some(vector) => write!(how, "MSI vector {vector:#x}"),
        None => how.write_str("polled"),
    };
    info!(
        "Disk: AHCI {major}.{minor} controller at {}: {ports} {}, {slots} command slots{}, {how}",
        function.address(),
        if ports == 1 { "port" } else { "ports" },
        if ncq { ", NCQ" } else { "" },
    );
    // Room for the ports while their links come up, a few kilobytes: on
    // the heap, built a port at a time.
    let mut ports = Vec::new_in(HEAP.with(Flags::MAY_SLEEP));
    ports.try_reserve_exact(PORTS).map_err(|_| "out of memory")?;
    while ports.len() < PORTS {
        let _ = ports.push_within_capacity(None);
    }
    let mut alloc = |pages: usize| <Arch as Devices>::alloc_dma(pages, false);
    host.hba.probe(&mut ports, &mut alloc, &mut queue::delay, &mut |number, found| match found {
        Ok(port) => {
            if let Err(why) = add(&host, number, port, next, interrupt.is_some()) {
                warn!("Disk: AHCI port {number} at {}: {why}", function.address());
            }
        }
        Err(Absent::NoLink) => {
            info!(
                "Disk: AHCI port {number} at {}: a device whose link did not come up",
                function.address()
            );
        }
        Err(why) => info!("Disk: AHCI port {number} at {}: {why}; not used", function.address()),
    });
    Ok(())
}

/// Sets up the disk on port `number` of `host`, named after letter `next`.
fn add(
    host: &Host,
    number: usize,
    port: Port,
    next: &mut usize,
    interrupt: bool,
) -> Result<(), &'static str> {
    if *next >= 26 {
        return Err("too many SATA disks");
    }
    let mut name = Name::new();
    let _ = write!(name, "sd{}", (b'a' + *next as u8) as char);
    let found = port.info();
    let identity = port.identity().copied();
    let info = Info {
        sectors: found.sectors,
        block_size: found.block_size,
        read_only: false,
        max_segments: radix_ahci::MAX_SEGMENTS,
        discard_sectors: found.trim_sectors,
        // A disk that does not say spins, as Linux takes it.
        rotational: found.rotational.unwrap_or(true),
    };
    let controller = SataPort { port, owner: [0; PORTS], name: name.clone() };
    let controller = queue::part(controller).ok_or("out of memory")?;
    let disk = Disk::new(
        core::iter::once(controller),
        info,
        found.write_cache,
        found.fua,
        interrupt,
        name.clone(),
    )
    .ok_or("out of memory")?;
    let disk = Arc::try_new_in(disk, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    let _ = host.disks[number].set(disk.clone());
    *next += 1;
    let model = identity.as_ref().map_or("", |id| trimmed(&id.model));
    let mut queued = TextBuf::<24>::new();
    if found.queued {
        let _ = write!(queued, ", NCQ depth {}", found.depth);
    }
    info!(
        "Disk: {name} is a SATA disk of {} on AHCI port {number}: {model}, {} bytes a \
         block{queued}{}{}{}{}",
        Size(info.sectors * radix_block::SECTOR),
        info.block_size,
        if found.write_cache { ", write cache" } else { "" },
        if found.fua { ", FUA" } else { "" },
        if info.discard_sectors > 0 { ", TRIM" } else { "" },
        if info.rotational { "" } else { ", solid state" },
    );
    let volume = Volume::new(disk, 0, info);
    let device: DeviceRef =
        Arc::try_new_in(volume, HEAP.with(Flags::MAY_SLEEP)).map_err(|_| "out of memory")?;
    register(&name, device.clone());
    partitions(&name, &device);
    Ok(())
}
