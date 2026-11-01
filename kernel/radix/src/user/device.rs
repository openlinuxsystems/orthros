// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cards a driver program drives (Phase 12, radix-device(2), ADR 0198).
//!
//! The PCI scan leaves a card the kernel does not drive itself here
//! ([`leave`]), once an IOMMU isolates it: with `e1000=program`, the
//! e1000 and e1000e. Its bus mastering stays off until a capability to it
//! is opened. Spark holds the root device capability (`init`), which
//! lists the cards and opens a capability to one, one at a time: a
//! [`DeviceObject`]. Through it the driver maps the card's registers,
//! uncached, and DMA memory into its space, the card gets its MSI, whose
//! arrival [`wait`](radix_abi::device::WAIT) reports, and a network card
//! is attached to its interface through a link (`net::program`).
//!
//! When the capability's last copy goes the registry marks it closed, under
//! the store's lock, and the store's caller stops it once the lock has
//! gone ([`close_deferred`]), in this order, so that the card reaches
//! nothing it should not at any point: the link is taken from the
//! interface, the card's bus mastering and MSI are turned off, its
//! interrupt is freed, its pages leave the driver's space, and only then
//! is its DMA memory given back, and the card free to open again.
//!
//! Lock order: a device's state, then a network stack's, then an address
//! space's regions.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use radix_abi::device::{
    self as abi, AttachNetworkArgs, AttachNetworkResults, DMA_PAGES_MAX, DeviceInfo,
    EVENT_INTERRUPT, INTERRUPT_MSI, INTERRUPT_NONE, InfoArgs, InterruptArgs, InterruptResults,
    LIST, ListArgs, ListResults, MapDmaArgs, MapDmaResults, MapRegistersArgs, MapRegistersResults,
    NO_INTERFACE, OpenArgs, WaitArgs, WaitResults, WakeArgs,
};
use radix_abi::errors::Kernel;
use radix_cap::{ObjectKind, Rights, SlotRef};
use radix_hal::{Devices, UserAccess, UserMode};
use radix_hw::pci::{Bar, Function, Msi};
use radix_hw::{Dma, MsiMessage};
use radix_klog::info;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_sync::{Mutex, Once, SpinLock};

use super::memory::{Backing, SpaceObject, SpaceRef};
use super::methods::{Answer, answer};
use super::objects::{self, Found, Object};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;
use crate::domains::DomainRef;
use crate::net::program::{self, Pages};
use crate::sched::{self, wait};

/// A card's capability, shared.
pub type DeviceRef = Arc<DeviceObject, Kmalloc>;

/// A page.
const PAGE: u64 = radix_nic::PAGE as u64;
/// How long `open` waits for a card whose last capability went to stop.
const CLOSING_WAIT_NS: u64 = 1_000_000_000;
/// The PCI command register's bus-master bit.
const BUS_MASTER: u16 = 1 << 2;
/// The PCI command register's memory-space bit.
const MEMORY_SPACE: u16 = 1 << 1;

/// A card the kernel left for a program.
struct Left {
    function: Function,
    /// Its network card's number among the cards (`net0` is 0), if it is
    /// one.
    interface: Option<usize>,
    /// Its MSI capability, if it has one.
    msi: Option<Msi>,
    /// Each of its memory BARs a page or larger: address and size.
    bars: [Option<(u64, u64)>; 6],
    /// A capability to it is open, or closing.
    held: AtomicBool,
    /// The open capability's object, until it is stopped.
    open: SpinLock<Option<DeviceRef>>,
    /// What happened since its driver last asked: `EVENT_` bits.
    events: AtomicU32,
}

impl Left {
    /// The key its waiters wait on: its events, and its opening.
    fn key(&self) -> usize {
        core::ptr::from_ref(self) as usize
    }

    /// What it is, for `list` and `info`.
    fn info(&self) -> DeviceInfo {
        let f = &self.function;
        let a = f.address();
        let (class, subclass, interface_code) = f.class();
        DeviceInfo {
            address: u32::from(a.segment) << 16
                | u32::from(a.bus) << 8
                | u32::from(a.device) << 3
                | u32::from(a.function),
            vendor: f.vendor_id(),
            device: f.device_id(),
            subsystem_vendor: f.subsystem_vendor_id(),
            subsystem: f.subsystem_id(),
            revision: f.revision(),
            base_class: class,
            subclass,
            interface_code,
            interrupts: if self.msi.is_some() { INTERRUPT_MSI } else { INTERRUPT_NONE } as u8,
            held: u8::from(self.held.load(Ordering::Acquire)),
            reserved: 0,
            interface: self.interface.map_or(NO_INTERFACE, |n| n as u32),
            bars: self.bars.map(|bar| bar.map_or(0, |(_, size)| size)),
        }
    }
}

/// The cards left, in the order they were found.
static LEFT: [Once<Left>; abi::DEVICES_MAX as usize] =
    [const { Once::new() }; abi::DEVICES_MAX as usize];
/// A capability to a card was released: [`close_deferred`] has one to
/// stop.
static CLOSING: AtomicBool = AtomicBool::new(false);

/// The cards left so far.
fn left() -> impl Iterator<Item = &'static Left> {
    LEFT.iter().map_while(Once::get)
}

/// Leaves the card `function` for a program, the network card numbered
/// `interface` if it is one: its bus mastering goes off until a
/// capability to it is opened. Returns its place, which its interface's
/// card keeps; `None` if too many are left.
pub fn leave(function: &Function, interface: usize) -> Option<usize> {
    let at = LEFT.iter().position(|slot| slot.get().is_none())?;
    let mut bars = [None; 6];
    for (index, bar) in bars.iter_mut().enumerate() {
        if let Some(Bar::Memory { address, size, .. }) = function.bar(index)
            && size >= PAGE
        {
            *bar = Some((address, size));
        }
    }
    let command = function.command();
    function.set_command((command | MEMORY_SPACE) & !BUS_MASTER);
    let left = Left {
        function: function.clone(),
        interface: Some(interface),
        msi: Msi::find(function),
        bars,
        held: AtomicBool::new(false),
        open: SpinLock::new(None),
        events: AtomicU32::new(0),
    };
    LEFT[at].set(left).ok()?;
    Some(at)
}

/// Tells the driver of the card left at `at` of `events`, and wakes it:
/// from the card's interrupt, or the stack's flush. Safe in an interrupt
/// handler.
pub fn signal(at: usize, events: u32) {
    let Some(left) = LEFT.get(at).and_then(Once::get) else { return };
    left.events.fetch_or(events, Ordering::SeqCst);
    wait::wake_all(left.key());
}

/// The card's MSI handler: `at` is its place.
fn on_interrupt(at: usize) {
    signal(at, EVENT_INTERRUPT);
}

/// What a card's capability holds while it is open.
struct State {
    /// Set once it has been stopped: nothing more is made.
    stopped: bool,
    /// The address space its pages are mapped in, once one is.
    space: Option<Weak<SpaceObject, Kmalloc>>,
    /// Its DMA memory, and how many pages of it.
    dma: Vec<Dma, Kmalloc>,
    dma_pages: u64,
    /// Its link, once it is attached, and the pages it is charged.
    link: Option<Arc<Pages, Kmalloc>>,
    /// Who its memory is charged to, and how many pages.
    domain: Option<DomainRef>,
    charged: u64,
    /// Its interrupt's vector, once it has one.
    vector: Option<u32>,
}

/// A capability to a card: what its driver made with it.
pub struct DeviceObject {
    /// Its place among the cards left.
    at: usize,
    /// Its last capability has gone.
    closed: AtomicBool,
    state: Mutex<State>,
}

impl DeviceObject {
    fn left(&self) -> &'static Left {
        // Opened only for a card that is there, which stays.
        LEFT[self.at].get().expect("an open card is left")
    }

    /// Stops `device`'s card and takes back what it was given, in the
    /// order the module says. Called once, with no lock held.
    fn stop(device: &DeviceRef) {
        let left = device.left();
        let mut state = device.state.lock();
        state.stopped = true;
        if let (Some(number), Some(_)) = (left.interface, state.link.as_ref()) {
            program::detach(number, device.at);
        }
        let f = &left.function;
        f.set_command(f.command() & !BUS_MASTER);
        if let Some(msi) = &left.msi {
            msi.disable(f);
        }
        if let Some(vector) = state.vector.take() {
            <Arch as Devices>::free_interrupt(vector);
        }
        if let Some(space) = state.space.take().and_then(|space| space.upgrade()) {
            // Any of its parts stands for all of them.
            space.unmap_backing(&Backing::Device(DevicePages {
                device: device.clone(),
                part: Part::Registers(0),
            }));
        }
        // The card can reach none of it now.
        state.dma.clear();
        state.link = None;
        if let Some(domain) = state.domain.take() {
            domain.uncharge(state.charged);
        }
        state.charged = 0;
        drop(state);
        left.events.store(0, Ordering::SeqCst);
        info!("Device: {} stopped, as its driver let go of it", f.address());
    }

    /// Charges `pages` pages to the caller's domain, the one its memory is
    /// charged to from its first page on.
    fn charge(state: &mut State, pages: u64) -> Result<()> {
        let domain = match &state.domain {
            Some(domain) => domain.clone(),
            None => sched::current().map(|me| me.domain().clone()).ok_or(error(Kernel::Gone))?,
        };
        domain.charge(pages).map_err(|_| error(Kernel::NoMemory))?;
        state.charged += pages;
        state.domain = Some(domain);
        Ok(())
    }

    /// Gives back the charge of `pages` pages.
    fn uncharge(state: &mut State, pages: u64) {
        if let Some(domain) = &state.domain {
            domain.uncharge(pages);
            state.charged -= pages;
        }
    }
}

/// Which part of a card a region maps.
#[derive(Clone)]
pub enum Part {
    /// Registers, from this physical address.
    Registers(u64),
    /// DMA memory, from this physical address.
    Dma(u64),
    /// Its network link.
    Link(Arc<Pages, Kmalloc>),
}

/// A part of a card, as a region of its driver's space maps it.
#[derive(Clone)]
pub struct DevicePages {
    device: DeviceRef,
    part: Part,
}

impl DevicePages {
    /// The physical address of its page `page`.
    pub fn phys(&self, page: u64) -> u64 {
        match &self.part {
            Part::Registers(base) | Part::Dma(base) => base + page * PAGE,
            Part::Link(pages) => pages.phys(page as usize),
        }
    }

    /// Where its pages start, if they follow one another.
    pub fn contiguous(&self) -> Option<u64> {
        match self.part {
            Part::Dma(base) => Some(base),
            Part::Registers(_) | Part::Link(_) => None,
        }
    }

    /// Whether its pages are mapped uncached: registers.
    pub fn uncached(&self) -> bool {
        matches!(self.part, Part::Registers(_))
    }

    /// Whether it is a part of the same card's capability.
    pub fn same_device(&self, other: &DevicePages) -> bool {
        Arc::ptr_eq(&self.device, &other.device)
    }
}

/// The capability's last copy went: it is stopped once the store's lock
/// has gone ([`close_deferred`]). Under the store's lock.
pub fn released(device: &DeviceObject) {
    device.closed.store(true, Ordering::Release);
    CLOSING.store(true, Ordering::Release);
}

/// Stops the cards whose capabilities were released. Called with no
/// lock held.
pub fn close_deferred() {
    if !CLOSING.swap(false, Ordering::AcqRel) {
        return;
    }
    for left in left() {
        let closed = {
            let open = left.open.lock();
            open.as_ref().filter(|device| device.closed.load(Ordering::Acquire)).cloned()
        };
        let Some(device) = closed else { continue };
        DeviceObject::stop(&device);
        *left.open.lock() = None;
        left.held.store(false, Ordering::Release);
        wait::wake_all(left.key());
    }
}

/// The object `found`, a device capability, designates.
fn object(found: &Found) -> Result<&Object> {
    found.object.as_ref().ok_or(error(Kernel::Gone))
}

/// The card `found` designates, and its rights checked.
fn card(found: &Found, rights: Rights) -> Result<&DeviceRef> {
    found.cap.check(ObjectKind::Device, rights).map_err(cap_error)?;
    match object(found)? {
        Object::Device(device) => Ok(device),
        // The root answers only `list` and `open`.
        _ => Err(error(Kernel::NotSupported)),
    }
}

/// The address space in slot `slot`, if it may be mapped into, and it is
/// the one the card's pages are in, or the first.
fn space_for(user: &UserRef, state: &mut State, slot: u32) -> Result<SpaceRef> {
    let (_, space) = objects::space(user.cspace, slot, Rights::CONTROL)?;
    match state.space.as_ref().and_then(Weak::upgrade) {
        Some(mapped) if !Arc::ptr_eq(&mapped, &space) => {
            return Err(error(Kernel::InvalidArgument));
        }
        Some(_) => {}
        None => state.space = Some(Arc::downgrade(&space)),
    }
    Ok(space)
}

/// Checks that `pages` pages from `address` are in the user half and
/// page-aligned, or that `address` is 0, for where the kernel chooses.
fn check_range(address: u64, pages: u64) -> Result<()> {
    if pages == 0
        || (address != 0
            && (!address.is_multiple_of(PAGE)
                || radix_ipc::range::user_pages(address, pages).is_none()))
    {
        return Err(error(Kernel::InvalidArgument));
    }
    Ok(())
}

/// Writes `infos` into the user's `buffer`, as many as fit.
fn write_infos(address: u64, len: u64, infos: impl Iterator<Item = DeviceInfo>) -> Result<()> {
    let room = usize::try_from(len).unwrap_or(usize::MAX) / DeviceInfo::SIZE;
    for (index, info) in infos.take(room).enumerate() {
        let at = address + (index * DeviceInfo::SIZE) as u64;
        let bytes = encode(&info);
        if !radix_ipc::range::user_bytes(at, bytes.len() as u64) || !Arch::copy_to_user(at, &bytes)
        {
            return Err(error(Kernel::BadAddress));
        }
    }
    Ok(())
}

/// `info`'s bytes, little-endian, at their offsets.
fn encode(info: &DeviceInfo) -> [u8; DeviceInfo::SIZE] {
    let mut out = [0u8; DeviceInfo::SIZE];
    let mut put = |at: usize, bytes: &[u8]| out[at..at + bytes.len()].copy_from_slice(bytes);
    put(DeviceInfo::OFFSET_ADDRESS, &info.address.to_le_bytes());
    put(DeviceInfo::OFFSET_VENDOR, &info.vendor.to_le_bytes());
    put(DeviceInfo::OFFSET_DEVICE, &info.device.to_le_bytes());
    put(DeviceInfo::OFFSET_SUBSYSTEM_VENDOR, &info.subsystem_vendor.to_le_bytes());
    put(DeviceInfo::OFFSET_SUBSYSTEM, &info.subsystem.to_le_bytes());
    put(DeviceInfo::OFFSET_REVISION, &[info.revision]);
    put(DeviceInfo::OFFSET_BASE_CLASS, &[info.base_class]);
    put(DeviceInfo::OFFSET_SUBCLASS, &[info.subclass]);
    put(DeviceInfo::OFFSET_INTERFACE_CODE, &[info.interface_code]);
    put(DeviceInfo::OFFSET_INTERRUPTS, &[info.interrupts]);
    put(DeviceInfo::OFFSET_HELD, &[info.held]);
    put(DeviceInfo::OFFSET_INTERFACE, &info.interface.to_le_bytes());
    for (index, bar) in info.bars.iter().enumerate() {
        put(DeviceInfo::OFFSET_BARS + 8 * index, &bar.to_le_bytes());
    }
    out
}

/// The PCI address `address` names, as `DeviceInfo.address` writes it.
fn left_at(address: u32) -> Option<(usize, &'static Left)> {
    left().enumerate().find(|(_, left)| left.info().address == address)
}

/// Opens a capability to the card at `address` in `to`, for `user`.
fn open(user: &UserRef, address: u32, to: u32) -> Result<()> {
    let (at, left) = left_at(address).ok_or(radix_abi::errors::File::NotFound.status(0))?;
    // One whose last capability went a moment ago may still be stopping.
    let deadline = sched::now_ns().saturating_add(CLOSING_WAIT_NS);
    let closing =
        || left.open.lock().as_ref().is_some_and(|device| device.closed.load(Ordering::Acquire));
    while left.held.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        if !closing() {
            return Err(error(Kernel::Busy));
        }
        let free = || !left.held.load(Ordering::Acquire);
        if !wait::wait_until_or_timeout(left.key(), "device", deadline, free) {
            return Err(error(Kernel::Busy));
        }
    }
    let device = Arc::try_new_in(
        DeviceObject {
            at,
            closed: AtomicBool::new(false),
            state: Mutex::new(State {
                stopped: false,
                space: None,
                dma: Vec::new_in(HEAP.with(Flags::MAY_SLEEP)),
                dma_pages: 0,
                link: None,
                domain: None,
                charged: 0,
                vector: None,
            }),
        },
        HEAP.with(Flags::MAY_SLEEP),
    );
    let Ok(device) = device else {
        left.held.store(false, Ordering::Release);
        return Err(error(Kernel::NoMemory));
    };
    *left.open.lock() = Some(device.clone());
    let made = super::process::install(
        SlotRef::new(user.cspace, to),
        ObjectKind::Device,
        Object::Device(device),
    );
    if let Err(err) = made {
        *left.open.lock() = None;
        left.held.store(false, Ordering::Release);
        return Err(err);
    }
    left.events.store(0, Ordering::SeqCst);
    let f = &left.function;
    f.set_command(f.command() | MEMORY_SPACE | BUS_MASTER);
    info!("Device: {} is open for {}", f.address(), user.process.name().as_str());
    Ok(())
}

/// Maps `bar` of `device`'s card at `address` in the space in `space`;
/// returns where, and its size.
fn map_registers(user: &UserRef, device: &DeviceRef, a: &MapRegistersArgs) -> Result<(u64, u64)> {
    let left = device.left();
    let bar = usize::try_from(a.bar).ok().and_then(|bar| left.bars.get(bar).copied().flatten());
    let (phys, size) = bar.ok_or(error(Kernel::InvalidArgument))?;
    let pages = size / PAGE;
    check_range(a.address, pages)?;
    let mut state = device.state.lock();
    if state.stopped {
        return Err(error(Kernel::Gone));
    }
    let space = space_for(user, &mut state, a.space)?;
    let backing =
        Backing::Device(DevicePages { device: device.clone(), part: Part::Registers(phys) });
    let at = space.map_placed(a.address, pages, &backing, 0, UserAccess::ReadWrite)?;
    Ok((at, size))
}

/// Gives `device`'s card `pages` pages of DMA memory at `address` in the
/// space in `space`; returns where, and where the card reaches them.
fn map_dma(user: &UserRef, device: &DeviceRef, a: &MapDmaArgs) -> Result<(u64, u64)> {
    let pages = u64::from(a.pages);
    check_range(a.address, pages)?;
    let mut state = device.state.lock();
    if state.stopped {
        return Err(error(Kernel::Gone));
    }
    if state.dma_pages + pages > u64::from(DMA_PAGES_MAX) {
        return Err(error(Kernel::Exhausted));
    }
    let space = space_for(user, &mut state, a.space)?;
    state.dma.try_reserve(1).map_err(|_| error(Kernel::NoMemory))?;
    DeviceObject::charge(&mut state, pages)?;
    let source = device.left().function.address();
    let Some(dma) = <Arch as Devices>::alloc_dma(source, pages as usize, false) else {
        DeviceObject::uncharge(&mut state, pages);
        return Err(error(Kernel::NoMemory));
    };
    let phys = dma.phys();
    let backing = Backing::Device(DevicePages { device: device.clone(), part: Part::Dma(phys) });
    let at = match space.map_placed(a.address, pages, &backing, 0, UserAccess::ReadWrite) {
        Ok(at) => at,
        Err(err) => {
            drop(dma);
            DeviceObject::uncharge(&mut state, pages);
            return Err(err);
        }
    };
    // Room was reserved above.
    let _ = state.dma.push_within_capacity(dma);
    state.dma_pages += pages;
    Ok((at, phys))
}

/// Gives `device`'s card its MSI, if it has one.
fn interrupt(device: &DeviceRef) -> Result<u32> {
    let left = device.left();
    let mut state = device.state.lock();
    if state.stopped {
        return Err(error(Kernel::Gone));
    }
    if state.vector.is_some() {
        return Err(error(Kernel::Already));
    }
    let Some(msi) = &left.msi else { return Ok(INTERRUPT_NONE) };
    let f = &left.function;
    let (vector, message): (u32, MsiMessage) =
        <Arch as Devices>::allocate_interrupt(f.address(), on_interrupt, device.at)
            .ok_or(error(Kernel::NoMemory))?;
    msi.enable(f, message);
    state.vector = Some(vector);
    info!("Device: {} has MSI vector {vector:#x}", f.address());
    Ok(INTERRUPT_MSI)
}

/// Attaches `device`'s card to its interface, its link mapped at
/// `address` in the space in `space`; returns where, and the interface's
/// index.
fn attach_network(user: &UserRef, device: &DeviceRef, a: &AttachNetworkArgs) -> Result<(u64, u32)> {
    let left = device.left();
    let number = left.interface.ok_or(error(Kernel::NotSupported))?;
    let mut mac = [0u8; 6];
    if a.mac.len != 6
        || !radix_ipc::range::user_bytes(a.mac.address, 6)
        || !Arch::copy_from_user(&mut mac, a.mac.address)
    {
        return Err(error(Kernel::InvalidArgument));
    }
    if !radix_nic::usable_mac(mac) {
        return Err(error(Kernel::InvalidArgument));
    }
    let pages = program::PAGES as u64;
    check_range(a.address, pages)?;
    let mut state = device.state.lock();
    if state.stopped {
        return Err(error(Kernel::Gone));
    }
    if state.link.is_some() {
        return Err(error(Kernel::Already));
    }
    let space = space_for(user, &mut state, a.space)?;
    DeviceObject::charge(&mut state, pages)?;
    let made =
        Pages::new().and_then(|link| Arc::try_new_in(link, HEAP.with(Flags::MAY_SLEEP)).ok());
    let Some(link) = made else {
        DeviceObject::uncharge(&mut state, pages);
        return Err(error(Kernel::NoMemory));
    };
    let part = Part::Link(link.clone());
    let backing = Backing::Device(DevicePages { device: device.clone(), part });
    let at = match space.map_placed(a.address, pages, &backing, 0, UserAccess::ReadWrite) {
        Ok(at) => at,
        Err(err) => {
            DeviceObject::uncharge(&mut state, pages);
            return Err(err);
        }
    };
    let name = user.process.name();
    if !program::attach(number, device.at, link.clone(), mac, name.as_str()) {
        let _ = space.unmap(at, pages);
        DeviceObject::uncharge(&mut state, pages);
        return Err(error(Kernel::Already));
    }
    state.link = Some(link);
    Ok((at, number as u32))
}

/// Waits for `device`'s card's events, up to `timeout_ns`.
fn wait_events(device: &DeviceRef, timeout_ns: u64) -> Result<u32> {
    let left = device.left();
    let deadline = sched::now_ns().saturating_add(timeout_ns);
    loop {
        let events = left.events.swap(0, Ordering::SeqCst);
        if events != 0 || timeout_ns == 0 {
            return Ok(events);
        }
        if sched::interrupted() {
            return Err(error(Kernel::Interrupted));
        }
        let ready = || left.events.load(Ordering::Relaxed) != 0 || sched::interrupted();
        if timeout_ns == u64::MAX {
            wait::wait_until(left.key(), "device", ready);
        } else if !wait::wait_until_or_timeout(left.key(), "device", deadline, ready) {
            return Ok(0);
        }
    }
}

/// Calls the method `label` on the device capability `found` for `user`:
/// kept out of line, as [`super::methods::call`] says.
#[inline(never)]
pub fn method(user: &UserRef, found: &Found, label: u32, args: &[u64; 8]) -> Result<Answer> {
    match label {
        LIST => {
            let a = ListArgs::decode(args)?;
            found.cap.check(ObjectKind::Device, Rights::READ).map_err(cap_error)?;
            if !matches!(object(found)?, Object::Devices) {
                return Err(error(Kernel::NotSupported));
            }
            write_infos(a.buffer.address, a.buffer.len, left().map(Left::info))?;
            let results = ListResults { count: left().count() as u32 };
            answer(label, ListResults::WORDS, results.encode())
        }
        abi::OPEN => {
            let a = OpenArgs::decode(args)?;
            found.cap.check(ObjectKind::Device, Rights::CONTROL).map_err(cap_error)?;
            if !matches!(object(found)?, Object::Devices) {
                return Err(error(Kernel::NotSupported));
            }
            open(user, a.address, a.to)?;
            answer(label, 0, [0; 8])
        }
        abi::INFO => {
            let a = InfoArgs::decode(args)?;
            let device = card(found, Rights::READ)?;
            if a.buffer.len < DeviceInfo::SIZE as u64 {
                return Err(error(Kernel::BadAddress));
            }
            write_infos(a.buffer.address, a.buffer.len, core::iter::once(device.left().info()))?;
            answer(label, 0, [0; 8])
        }
        abi::MAP_REGISTERS => {
            let a = MapRegistersArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            let (at, size) = map_registers(user, device, &a)?;
            answer(label, MapRegistersResults::WORDS, MapRegistersResults { at, size }.encode())
        }
        abi::MAP_DMA => {
            let a = MapDmaArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            let (at, device_address) = map_dma(user, device, &a)?;
            let results = MapDmaResults { at, device_address };
            answer(label, MapDmaResults::WORDS, results.encode())
        }
        abi::INTERRUPT => {
            InterruptArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            let kind = interrupt(device)?;
            answer(label, InterruptResults::WORDS, InterruptResults { kind }.encode())
        }
        abi::ATTACH_NETWORK => {
            let a = AttachNetworkArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            let (at, interface) = attach_network(user, device, &a)?;
            let results = AttachNetworkResults { at, interface };
            answer(label, AttachNetworkResults::WORDS, results.encode())
        }
        abi::WAIT => {
            let a = WaitArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            let events = wait_events(device, a.timeout_ns)?;
            answer(label, WaitResults::WORDS, WaitResults { events }.encode())
        }
        abi::WAKE => {
            WakeArgs::decode(args)?;
            let device = card(found, Rights::CONTROL)?;
            if let Some(number) = device.left().interface {
                program::wake(number);
            }
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}
