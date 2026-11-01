// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A USB 2 or USB 1.1 controller's thread: EHCI, OHCI or UHCI (Phase 11g,
//! ADR 0243).
//!
//! [`run`] takes the controller from the firmware and starts it
//! (`radix_usbhc`), then loops every 10 ms, as the kernel has no INTx
//! interrupts for these controllers yet: it reads each root port, and
//! sets up the device plugged into one or lets go of the one pulled out
//! (and of every device behind it, if it is a hub); it reads the hubs'
//! and the HID interfaces' interrupt pipes; and it lights the keyboards'
//! lock lights when consoled(8) changes them.
//!
//! Unlike an xHCI controller, these leave a device's address to the
//! kernel: a device found is reset, read at address 0 for its default
//! pipe's packet size, given the lowest address free on the bus
//! (SET_ADDRESS) and 2 ms to take it (USB 2.0 9.2.6.3), then read and
//! configured as on an xHCI controller (`controller::describe`), and
//! named as Linux names it, `2-1.3`. A low- or full-speed device on an
//! EHCI controller's root port is handed to its companion OHCI or UHCI
//! controller, whose own thread sets it up; behind a high-speed hub, it
//! stays on the EHCI controller, which talks to it through the hub's
//! Transaction Translator.
//!
//! Hubs and HID interfaces are driven as on xHCI (ADRs 0147, 0148), over
//! the controller's interrupt pipes: a hub's status pipe says which ports
//! changed, each queued port is read and its device set up or let go;
//! each HID report is decoded into `/devices/kbd` and `/devices/mouse`.
//! USB sticks are not read on these controllers yet: their mass storage
//! interface is named, and left alone.

use alloc::boxed::Box;
use alloc::vec::Vec;

use radix_hid::{BOOT_KEYBOARD, BOOT_MOUSE, Decoder, Layout};
use radix_klog::{TextBuf, error, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_usb::host::{self, Data, Endpoint, Hcd, PipeId, Target, Tt, Waiter};
use radix_usb::hub::{HubDescriptor, PortStatus, feature};
use radix_usb::{
    DeviceDescriptor, EndpointDescriptor, Setup, Speed, TransferType, class, descriptor, hid,
};
use radix_usbhc::Handoff;

use super::controller::{Name, Path, RECOVERY_NS, RESET_US, Why, alloc, read_device, translated};
use super::hid::{HidPlan, MAX_HIDS};
use super::shown::{self, RootHub, Shown};
use super::{Kind, STARTS, Start};
use crate::fs::keyboard;
use crate::hid::tell;
use crate::sched;

/// How often the thread looks at the controller.
const POLL_NS: u64 = 10_000_000;
/// How long a control transfer may take: Linux's 5 s.
const CONTROL_MS: u32 = 5_000;
/// How long a new connection is left to settle before its port's reset
/// (TATTDB, USB 2.0 7.1.7.3).
const DEBOUNCE_NS: u64 = 100_000_000;
/// How long a device is given to take its address (TSETADDR, USB 2.0
/// 9.2.6.3).
const ADDRESS_NS: u64 = 2_000_000;
/// The least wait after a hub's ports are powered.
const POWER_MS: u16 = 100;
/// How often a hub's port is read while it resets.
const RESET_POLL_US: u32 = 10_000;
/// Errors in a row on an interrupt pipe before it is given up.
const PIPE_ERRORS: u8 = 3;
/// Hub ports waiting to be looked at.
const WORK: usize = 64;
/// Where control transfers' data lands.
const BUFFER_SIZE: usize = 4096;
/// The largest interrupt transfer a pipe's buffer takes.
const REPORT_MAX: usize = 1024;

/// What the driver waits with: the thread's sleep.
struct KernelWaiter;

impl Waiter for KernelWaiter {
    fn wait(&mut self, us: u32) {
        sched::block_until(sched::now_ns().saturating_add(u64::from(us) * 1000), "usb");
    }

    fn now_us(&mut self) -> u64 {
        sched::now_ns() / 1000
    }
}

/// What the thread keeps of a hub.
struct Hub {
    /// The ports used: at most 15, which a route string reaches.
    ports: u8,
    /// Its status pipe, its endpoint's address, and the bytes of a change
    /// report.
    pipe: PipeId,
    endpoint: u8,
    report: usize,
    /// The ports whose device could not be set up, a bit each.
    failed: u16,
    errors: u8,
}

/// What the thread keeps of a HID interface it reads.
struct Hid {
    interface: u8,
    pipe: PipeId,
    /// Its interrupt IN endpoint's address, for CLEAR_FEATURE.
    endpoint: u8,
    length: usize,
    layout: Layout<Kmalloc>,
    decoder: Decoder,
    errors: u8,
    lights: Option<u32>,
}

/// A device the thread has set up.
struct Attached {
    path: Path,
    target: Target,
    max_packet: u16,
    name: TextBuf<64>,
    hub: Option<Hub>,
    hids: Vec<Hid, Kmalloc>,
}

/// A controller, as its thread runs it.
struct Legacy {
    bus: usize,
    hcd: Box<dyn Hcd + Send, Kmalloc>,
    waiter: KernelWaiter,
    devices: Vec<Attached, Kmalloc>,
    /// Addresses in use, a bit each, 1 to 127.
    addresses: u128,
    /// Root ports whose device could not be set up, until it is unplugged.
    failed: u32,
    buffer: Vec<u8, Kmalloc>,
    /// Hub ports to look at: the hub's address and the port.
    work: Vec<(u8, u8), Kmalloc>,
    lights: u32,
}

/// The thread of controller `index`, a USB 2 or USB 1.1 one.
pub fn run(index: usize) {
    let Some(start) = STARTS.get(index).and_then(radix_sync::Once::get) else { return };
    let controller = Legacy::start(index + 1, start);
    super::dbc::looked();
    let Some(mut controller) = controller else {
        super::looked();
        return;
    };
    for port in 1..=controller.hcd.ports() {
        controller.port_changed(port, true);
    }
    while controller.work() {}
    super::looked();
    loop {
        if super::halted() {
            // The machine goes down: what the ports say now is not news.
            return;
        }
        let busy =
            controller.ports() | controller.pipes() | controller.work() | controller.lights();
        if super::halted() {
            return;
        }
        if controller.hcd.failed() {
            for device in &controller.devices {
                shown::remove(controller.bus, device.path);
            }
            error!(
                "USB: bus {}: the controller stopped with an error; its devices are gone",
                controller.bus
            );
            return;
        }
        if !busy {
            sched::block_until(sched::now_ns().saturating_add(POLL_NS), "usb");
        }
    }
}

/// A started driver, and what to log of it.
type Started = (Box<dyn Hcd + Send, Kmalloc>, TextBuf<96>, Handoff);

/// Starts the driver of the controller `start` names.
fn start_driver(
    start: &Start,
    waiter: &mut KernelWaiter,
) -> Result<(Box<dyn Hcd + Send, Kmalloc>, TextBuf<96>), radix_usbhc::Error> {
    // Each in its own frame: the drivers are large while they start.
    let (driver, mut about, handoff) = match start.kind {
        Kind::Ehci => start_ehci(start, waiter)?,
        Kind::Ohci => start_ohci(start, waiter)?,
        Kind::Uhci => start_uhci(start, waiter)?,
        Kind::Xhci => return Err(radix_usbhc::Error::Unsupported("it is an xHCI controller")),
    };
    let _ = core::fmt::Write::write_str(
        &mut about,
        match handoff {
            Handoff::Given => ", polled, taken from the firmware",
            Handoff::Taken => ", polled, taken from the firmware, which would not let go",
            Handoff::None | Handoff::Free => ", polled",
        },
    );
    Ok((driver, about))
}

#[inline(never)]
fn start_ehci(start: &Start, waiter: &mut KernelWaiter) -> Result<Started, radix_usbhc::Error> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    // Built on the heap from the constant, not on this stack.
    let tables = Box::try_new_in(radix_usbhc::ehci::Tables::EMPTY, heap)
        .map_err(|_| radix_usbhc::Error::NoMemory)?;
    let regs = start.regs.memory().ok_or(radix_usbhc::Error::Unsupported("no registers"))?;
    let ehci = radix_usbhc::ehci::Ehci::new(
        &start.function,
        regs,
        waiter,
        &mut alloc(start.address),
        tables,
    )?;
    let info = ehci.info();
    let mut about = TextBuf::<96>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut about,
        format_args!(
            "EHCI {:x}.{:x} controller at {}: {} ports, {} companion controllers",
            info.version >> 8,
            (info.version & 0xff) >> 4,
            start.address,
            info.ports,
            info.companions
        ),
    );
    let boxed = Box::try_new_in(ehci, heap).map_err(|_| radix_usbhc::Error::NoMemory)?;
    Ok((boxed, about, info.handoff))
}

#[inline(never)]
fn start_ohci(start: &Start, waiter: &mut KernelWaiter) -> Result<Started, radix_usbhc::Error> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let tables = Box::try_new_in(radix_usbhc::ohci::Tables::EMPTY, heap)
        .map_err(|_| radix_usbhc::Error::NoMemory)?;
    let regs = start.regs.memory().ok_or(radix_usbhc::Error::Unsupported("no registers"))?;
    let ohci =
        radix_usbhc::ohci::Ohci::new(regs.clone(), waiter, &mut alloc(start.address), tables)?;
    let info = ohci.info();
    let mut about = TextBuf::<96>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut about,
        format_args!(
            "OHCI {:x}.{:x} controller at {}: {} ports",
            info.revision >> 4,
            info.revision & 0xf,
            start.address,
            info.ports
        ),
    );
    let boxed = Box::try_new_in(ohci, heap).map_err(|_| radix_usbhc::Error::NoMemory)?;
    Ok((boxed, about, info.handoff))
}

#[inline(never)]
fn start_uhci(start: &Start, waiter: &mut KernelWaiter) -> Result<Started, radix_usbhc::Error> {
    let heap = HEAP.with(Flags::MAY_SLEEP);
    let super::Window::Ports(io) = start.regs else {
        return Err(radix_usbhc::Error::Unsupported("no I/O ports"));
    };
    let tables = Box::try_new_in(radix_usbhc::uhci::Tables::EMPTY, heap)
        .map_err(|_| radix_usbhc::Error::NoMemory)?;
    let uhci = radix_usbhc::uhci::Uhci::new(
        &start.function,
        io,
        waiter,
        &mut alloc(start.address),
        tables,
    )?;
    let info = uhci.info();
    let mut about = TextBuf::<96>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut about,
        format_args!("UHCI controller at {}: {} ports", start.address, info.ports),
    );
    let boxed = Box::try_new_in(uhci, heap).map_err(|_| radix_usbhc::Error::NoMemory)?;
    Ok((boxed, about, info.handoff))
}

impl Legacy {
    #[inline(never)]
    fn start(bus: usize, start: &Start) -> Option<Box<Legacy, Kmalloc>> {
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let mut buffer = Vec::new_in(heap);
        let mut work = Vec::new_in(heap);
        if buffer.try_reserve_exact(BUFFER_SIZE).is_err() || work.try_reserve_exact(WORK).is_err() {
            error!("USB: out of memory for the controller at {}", start.address);
            return None;
        }
        while buffer.push_within_capacity(0).is_ok() {}
        let mut waiter = KernelWaiter;
        let (hcd, about) = match start_driver(start, &mut waiter) {
            Ok(started) => started,
            Err(err) => {
                warn!("USB: the controller at {} could not be started: {err}", start.address);
                return None;
            }
        };
        info!("USB: bus {bus} is the {about}");
        let ports = hcd.ports();
        let (product, speed, version) = match start.kind {
            Kind::Ehci => ("EHCI Host Controller", "480", " 2.00"),
            Kind::Ohci => ("OHCI PCI host controller", "12", " 1.10"),
            Kind::Uhci => ("UHCI Host Controller", "12", " 1.10"),
            Kind::Xhci => ("xHCI Host Controller", "5000", " 3.00"),
        };
        let root = RootHub { ports, address: start.address, product, speed, version };
        shown::add_root_hub(bus, root);
        let legacy = Legacy {
            bus,
            hcd,
            waiter,
            devices: Vec::new_in(heap),
            addresses: 1,
            failed: 0,
            buffer,
            work,
            lights: keyboard::lights(),
        };
        Box::try_new_in(legacy, heap).ok()
    }

    /// Reads every root port; says whether one changed.
    fn ports(&mut self) -> bool {
        let mut any = false;
        for port in 1..=self.hcd.ports() {
            if super::halted() {
                break;
            }
            let status = self.hcd.port(port);
            if status.connect_changed || status.other_changed {
                any = true;
                self.port_changed(port, false);
            }
        }
        any
    }

    /// Root port `port` may have changed (or, at `first`, has its device
    /// from before the controller started): lets go of a device that
    /// left, and sets up one that came.
    #[inline(never)]
    fn port_changed(&mut self, port: u8, first: bool) {
        let status = self.hcd.port(port);
        self.hcd.acknowledge(port);
        let path = Path { root_port: port, route: 0 };
        let bit = 1u32 << (port % 32);
        if status.connect_changed || !status.connected {
            self.failed &= !bit;
            self.detach(path);
        }
        if status.over_current {
            warn!("USB: {}: the port draws too much current", Name(self.bus, path));
        }
        let present = self.devices.iter().any(|d| d.path == path);
        if !status.connected || status.companion || present || self.failed & bit != 0 {
            return;
        }
        if !first {
            sched::sleep_ns(DEBOUNCE_NS);
        }
        for attempt in 0..2 {
            let mut name = TextBuf::<64>::new();
            let result = match self.hcd.reset(port, &mut self.waiter) {
                Ok(Some(speed)) => {
                    sched::sleep_ns(RECOVERY_NS);
                    self.set_up(path, Target { address: 0, speed, tt: None }, &mut name)
                }
                Ok(None) => {
                    info!(
                        "USB: {}: a low- or full-speed device, handed to the companion controller",
                        Name(self.bus, path)
                    );
                    return;
                }
                Err(err) => Err(Why::Host(err)),
            };
            let Err(why) = result else { return };
            if !self.hcd.port(port).connected {
                self.pulled_out(path, &name);
                return;
            }
            if attempt == 1 || matches!(why, Why::Host(host::Error::NoCompanion)) {
                warn!("USB: {}: the device could not be set up: {why}", Name(self.bus, path));
                self.failed |= bit;
                return;
            }
        }
    }

    fn pulled_out(&self, path: Path, name: &TextBuf<64>) {
        if !name.is_empty() {
            info!("USB: {}: {name} was unplugged", Name(self.bus, path));
        }
    }

    /// A free address, taken.
    fn take_address(&mut self) -> Option<u8> {
        let free = (1..128u8).find(|&a| self.addresses & (1 << a) == 0)?;
        self.addresses |= 1 << free;
        Some(free)
    }

    /// A control transfer on the default pipe of `target`.
    fn control(
        &mut self,
        target: Target,
        max_packet: u16,
        setup: Setup,
        data: &mut [u8],
    ) -> Result<usize, Why> {
        Ok(self.hcd.control(target, max_packet, setup, data, CONTROL_MS, &mut self.waiter)?)
    }

    /// A control transfer without data to the device at `at`.
    fn control_out(&mut self, at: usize, setup: Setup) -> Result<(), Why> {
        let (target, max) = (self.devices[at].target, self.devices[at].max_packet);
        self.control(target, max, setup, &mut []).map(|_| ())
    }

    /// Gives the device just reset at `path` (at address 0, `target`) its
    /// address, reads what it is and configures it; a hub or HID
    /// interfaces are then started.
    #[inline(never)]
    fn set_up(&mut self, path: Path, target: Target, name: &mut TextBuf<64>) -> Result<(), Why> {
        if self.devices.try_reserve(1).is_err() {
            return Err(Why::Device("out of memory"));
        }
        let speed = target.speed;
        let mut first = [0u8; 8];
        let max0 = speed.default_max_packet();
        let setup = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
        let came = self.control(target, max0, setup, &mut first);
        self.hcd.forget(0, &mut self.waiter);
        came?;
        let max = DeviceDescriptor::parse(&first)
            .and_then(|d| d.max_packet0(speed))
            .ok_or(Why::Device("it says a packet size its speed does not allow"))?;
        let address = self.take_address().ok_or(Why::Device("the bus has no address left"))?;
        let given = self.control(target, max, Setup::set_address(address), &mut []);
        self.hcd.forget(0, &mut self.waiter);
        if let Err(why) = given {
            self.addresses &= !(1 << address);
            return Err(why);
        }
        sched::sleep_ns(ADDRESS_NS);
        let target = Target { address, ..target };
        let result = self.configure(path, target, max, name);
        let configured = match result {
            Ok(done) => done,
            Err(why) => {
                name.clear();
                self.hcd.forget(address, &mut self.waiter);
                self.addresses &= !(1 << address);
                return Err(why);
            }
        };
        let (plan, shown) = &*configured;
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let attached = Attached {
            path,
            target,
            max_packet: max,
            name: name.clone(),
            hub: None,
            hids: Vec::new_in(heap),
        };
        let _ = self.devices.push_within_capacity(attached);
        if !shown::add(shown) {
            warn!("USB: {}: too many devices to show in /kernel/bus/usb", Name(self.bus, path));
        }
        if let Some(status) = plan.hub {
            match self.start_hub(path, status) {
                Ok(()) => {
                    let hubs =
                        shown.interfaces().iter().filter(|i| i.descriptor.class == class::HUB);
                    for i in hubs {
                        shown::set_driver(self.bus, path, i.descriptor.number, "hub");
                    }
                }
                Err(why) => {
                    warn!("USB: {}: the hub could not be started: {why}", Name(self.bus, path));
                }
            }
        }
        for &interface in plan.hids() {
            match self.start_hid(path, interface) {
                Ok(()) => shown::set_driver(self.bus, path, interface.interface, "usbhid"),
                Err(why) => warn!(
                    "USB: {}: interface {} could not be read: {why}",
                    Name(self.bus, path),
                    interface.interface
                ),
            }
        }
        for port in plan.serials() {
            warn!(
                "USB: {}: interface {}: serial ports on {} controllers are not carried yet",
                Name(self.bus, path),
                port.data,
                self.hcd.kind()
            );
        }
        if let Some(storage) = plan.storage {
            warn!(
                "USB: {}: interface {}: disks on {} controllers are not read yet",
                Name(self.bus, path),
                storage.interface,
                self.hcd.kind()
            );
        }
        Ok(())
    }

    /// Reads what the device at `target` is and puts it in its first
    /// configuration. Kept out of [`Legacy::set_up`]'s frame.
    #[inline(never)]
    fn configure(
        &mut self,
        path: Path,
        target: Target,
        max: u16,
        name: &mut TextBuf<64>,
    ) -> Result<Box<(super::controller::Plan, Shown), Kmalloc>, Why> {
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let Legacy { hcd, waiter, buffer, bus, .. } = self;
        let mut control =
            |setup, out: &mut [u8]| Ok(hcd.control(target, max, setup, out, CONTROL_MS, waiter)?);
        let mut shown = None;
        let plan = read_device(&mut control, buffer, (*bus, path, target.speed), name, &mut shown)?;
        control(Setup::set_configuration(plan.value), &mut [])?;
        let shown = shown.ok_or(Why::Device("it was not read"))?;
        Box::try_new_in((plan, shown), heap).map_err(|_| Why::Device("out of memory"))
    }

    /// Where the device at `path` is in [`Legacy::devices`].
    fn find(&self, path: Path) -> Option<usize> {
        self.devices.iter().position(|d| d.path == path)
    }

    /// Lets go of the device at `path` and of every device below it.
    fn detach(&mut self, path: Path) {
        while let Some(at) =
            self.devices.iter().position(|d| d.path == path || d.path.is_below(path))
        {
            let mut gone = self.devices.remove(at);
            info!("USB: {}: {} was unplugged", Name(self.bus, gone.path), gone.name);
            shown::remove(self.bus, gone.path);
            for hid in &mut gone.hids {
                hid.decoder.release_all(&mut tell);
                self.hcd.close(hid.pipe, &mut self.waiter);
            }
            if let Some(hub) = &gone.hub {
                self.hcd.close(hub.pipe, &mut self.waiter);
            }
            let address = gone.target.address;
            self.hcd.forget(address, &mut self.waiter);
            self.addresses &= !(1 << address);
            self.work.retain(|&(hub, _)| hub != address);
        }
    }

    /// Opens an interrupt IN pipe to `endpoint`, whose transfers are
    /// `length` bytes, and starts its first.
    fn listen(&mut self, endpoint: Endpoint, length: usize) -> Result<PipeId, Why> {
        let pipe = self.hcd.open(endpoint, length)?;
        if let Err(err) = self.hcd.start(pipe, Data::In(length)) {
            self.hcd.close(pipe, &mut self.waiter);
            return Err(err.into());
        }
        Ok(pipe)
    }

    /// Looks at every interrupt pipe; says whether one finished.
    fn pipes(&mut self) -> bool {
        let mut any = false;
        let mut at = 0;
        while at < self.devices.len() {
            let path = self.devices[at].path;
            if let Some(pipe) = self.devices[at].hub.as_ref().map(|h| h.pipe)
                && let Some(result) = self.hcd.poll(pipe)
            {
                any = true;
                self.hub_status(at, pipe, result);
            }
            // A hub's change may have taken devices away.
            let Some(at_now) = self.find(path) else { continue };
            at = at_now;
            for index in 0..self.devices[at].hids.len() {
                let pipe = self.devices[at].hids[index].pipe;
                if let Some(result) = self.hcd.poll(pipe) {
                    any = true;
                    self.hid_report(at, index, result);
                }
            }
            at += 1;
        }
        any
    }

    /// Starts the transfer on `pipe` again, or gives the pipe up after too
    /// many errors in a row: `errors` counts them, and `endpoint` is the
    /// address whose halt a stall clears.
    fn again(
        &mut self,
        at: usize,
        pipe: PipeId,
        length: usize,
        result: Result<usize, host::Error>,
        (errors, endpoint): (&mut u8, u8),
    ) -> bool {
        match result {
            Ok(_) => *errors = 0,
            Err(err) => {
                *errors += 1;
                if *errors > PIPE_ERRORS {
                    let path = self.devices[at].path;
                    warn!(
                        "USB: {}: its endpoint {endpoint:#04x} stopped: {err}",
                        Name(self.bus, path)
                    );
                    return false;
                }
                if err == host::Error::Stall {
                    let _ = self.control_out(at, Setup::clear_halt(endpoint));
                }
                self.hcd.reset_pipe(pipe);
            }
        }
        self.hcd.start(pipe, Data::In(length)).is_ok()
    }

    /// Looks at the hub ports queued; says whether there were any.
    fn work(&mut self) -> bool {
        if self.work.is_empty() {
            return false;
        }
        while !self.work.is_empty() {
            let (hub, port) = self.work.remove(0);
            self.hub_port(hub, port);
        }
        true
    }

    /// Queues port `port` of the hub at address `hub`.
    fn queue(&mut self, hub: u8, port: u8) {
        if !self.work.contains(&(hub, port)) && self.work.push_within_capacity((hub, port)).is_err()
        {
            warn!("USB: bus {}: too many hub ports to look at; one is skipped", self.bus);
        }
    }

    /// Lights every keyboard's lock lights as consoled(8) last asked, if
    /// that changed; says whether it did.
    fn lights(&mut self) -> bool {
        let lights = keyboard::lights();
        if lights == self.lights {
            return false;
        }
        self.lights = lights;
        for at in 0..self.devices.len() {
            self.hid_lights(at);
        }
        true
    }
}

/// Hubs.
impl Legacy {
    /// Starts the configured hub at `path`, whose changes `status` reports.
    #[inline(never)]
    fn start_hub(&mut self, path: Path, status: EndpointDescriptor) -> Result<(), Why> {
        let at = self.find(path).ok_or(Why::Device("it is gone"))?;
        let (target, max) = (self.devices[at].target, self.devices[at].max_packet);
        let size = usize::from(HubDescriptor::MAX_SIZE);
        let mut bytes = [0u8; HubDescriptor::MAX_SIZE as usize];
        let came =
            self.control(target, max, Setup::hub_descriptor(false, size as u16), &mut bytes)?;
        let descriptor = HubDescriptor::parse(&bytes[..came.min(size)], false)
            .ok_or(Why::Device("it sent no hub descriptor"))?;
        let ports = descriptor.usable_ports();
        for port in 1..=ports {
            self.control_out(at, Setup::set_port_feature(port, feature::PORT_POWER))?;
        }
        info!("USB: {}: a hub of {} ports", Name(self.bus, path), descriptor.ports);
        shown::set_hub_ports(self.bus, path, descriptor.ports);
        if descriptor.ports > ports {
            warn!("USB: {}: only its first {ports} ports are used", Name(self.bus, path));
        }
        if path.child(1).is_none() {
            warn!(
                "USB: {}: {} hubs deep; what is plugged into it is not used",
                Name(self.bus, path),
                path.depth() + 1
            );
            return Ok(());
        }
        sched::sleep_ns(u64::from(descriptor.power_good_ms.max(POWER_MS)) * 1_000_000);
        let report = descriptor.report_size().min(usize::from(status.max_packet()).max(1));
        let endpoint = Endpoint {
            target,
            number: status.number(),
            into_host: true,
            kind: TransferType::Interrupt,
            max_packet: status.max_packet(),
            transactions: 1,
            interval: status.interval,
        };
        let pipe = self.listen(endpoint, report)?;
        let address = status.address;
        self.devices[at].hub =
            Some(Hub { ports, pipe, endpoint: address, report, failed: 0, errors: 0 });
        for port in 1..=ports {
            self.queue(target.address, port);
        }
        Ok(())
    }

    /// The transfer on the status pipe of the hub at `at` ended.
    fn hub_status(&mut self, at: usize, pipe: PipeId, result: Result<usize, host::Error>) {
        let address = self.devices[at].target.address;
        let Some(hub) = self.devices[at].hub.as_mut() else { return };
        let (ports, report, status_endpoint) = (hub.ports, hub.report, hub.endpoint);
        let mut errors = hub.errors;
        if let Ok(came) = result {
            let mut bits = [0u8; 2];
            let came = came.min(bits.len());
            self.hcd.received(pipe, &mut bits[..came]);
            for port in 1..=ports {
                if bits[usize::from(port / 8)] & (1 << (port % 8)) != 0 {
                    self.queue(address, port);
                }
            }
        }
        let going = self.again(at, pipe, report, result, (&mut errors, status_endpoint));
        if let Some(hub) = self.devices[at].hub.as_mut() {
            hub.errors = errors;
            if !going {
                self.hcd.close(pipe, &mut self.waiter);
                self.devices[at].hub = None;
            }
        }
    }

    /// The status of port `port` of the hub at `at`.
    fn port_status(&mut self, at: usize, port: u8) -> Result<PortStatus, Why> {
        let (target, max) = (self.devices[at].target, self.devices[at].max_packet);
        let mut bytes = [0u8; PortStatus::SIZE as usize];
        let came = self.control(target, max, Setup::port_status(port), &mut bytes)?;
        PortStatus::parse(&bytes[..came], false)
            .ok_or(Why::Device("its hub's port status is short"))
    }

    /// Port `port` of the hub at address `hub` may have changed.
    #[inline(never)]
    fn hub_port(&mut self, hub: u8, port: u8) {
        let Some(at) = self.devices.iter().position(|d| d.target.address == hub && d.hub.is_some())
        else {
            return;
        };
        let hub_path = self.devices[at].path;
        let Some(path) = hub_path.child(port) else { return };
        let Ok(status) = self.port_status(at, port) else { return };
        for change in status.changes() {
            let _ = self.control_out(at, Setup::clear_port_feature(port, change));
        }
        if status.connect_changed() || !status.connected() {
            self.set_hub_failed(hub_path, port, false);
            self.detach(path);
        }
        if status.over_current_changed() && status.over_current() {
            warn!("USB: {}: the port draws too much current", Name(self.bus, path));
        }
        let Some(at) = self.find(hub_path) else { return };
        let failed = self.devices[at].hub.as_ref().is_some_and(|h| h.failed & (1 << port) != 0);
        if !status.connected() || failed || self.find(path).is_some() {
            return;
        }
        sched::sleep_ns(DEBOUNCE_NS);
        for attempt in 0..2 {
            let mut name = TextBuf::<64>::new();
            let Err(why) = self.attach_behind(hub_path, port, path, &mut name) else { return };
            let connected = self
                .find(hub_path)
                .and_then(|at| self.port_status(at, port).ok())
                .is_some_and(|s| s.connected());
            if !connected {
                self.pulled_out(path, &name);
                return;
            }
            if attempt == 1 {
                warn!("USB: {}: the device could not be set up: {why}", Name(self.bus, path));
                self.set_hub_failed(hub_path, port, true);
                return;
            }
        }
    }

    fn set_hub_failed(&mut self, hub: Path, port: u8, failed: bool) {
        let Some(at) = self.find(hub) else { return };
        if let Some(hub) = &mut self.devices[at].hub {
            if failed {
                hub.failed |= 1 << port;
            } else {
                hub.failed &= !(1 << port);
            }
        }
    }

    /// Resets port `port` of the hub at `hub` and sets up its device, at
    /// `path`.
    fn attach_behind(
        &mut self,
        hub: Path,
        port: u8,
        path: Path,
        name: &mut TextBuf<64>,
    ) -> Result<(), Why> {
        let at = self.find(hub).ok_or(Why::Device("its hub is gone"))?;
        self.control_out(at, Setup::set_port_feature(port, feature::PORT_RESET))?;
        let deadline = self.waiter.now_us() + RESET_US;
        let status = loop {
            self.waiter.wait(RESET_POLL_US);
            let status = self.port_status(at, port)?;
            if status.reset_changed() && !status.resetting() {
                self.control_out(at, Setup::clear_port_feature(port, feature::C_PORT_RESET))?;
                break status;
            }
            if !status.connected() {
                return Err(Why::Device("it was unplugged"));
            }
            if self.waiter.now_us() >= deadline {
                return Err(Why::Device("its port did not finish its reset"));
            }
        };
        if !status.connected() {
            return Err(Why::Device("it was unplugged"));
        }
        if !status.enabled() {
            return Err(Why::Device("its port would not enable"));
        }
        sched::sleep_ns(RECOVERY_NS);
        let speed = status.speed();
        let parent = self.devices[at].target;
        // A slow device behind a high-speed hub is reached through that
        // hub's Transaction Translator; behind a slow hub, through the one
        // that hub is behind, if any.
        let tt = if !translated(speed) {
            None
        } else if parent.speed == Speed::High {
            Some(Tt { hub: parent.address, port })
        } else {
            parent.tt
        };
        self.set_up(path, Target { address: 0, speed, tt }, name)
    }
}

/// HID interfaces.
impl Legacy {
    /// Starts the HID interface `plan` of the configured device at `path`.
    #[inline(never)]
    fn start_hid(&mut self, path: Path, plan: HidPlan) -> Result<(), Why> {
        let at = self.find(path).ok_or(Why::Device("it is gone"))?;
        let (target, max) = (self.devices[at].target, self.devices[at].max_packet);
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let interface = plan.interface;
        let length = usize::from(plan.report_length).min(BUFFER_SIZE);
        let own = (length > 0)
            .then(|| {
                let Legacy { hcd, waiter, buffer, .. } = &mut *self;
                let setup = Setup::report_descriptor(interface, length as u16);
                let came =
                    hcd.control(target, max, setup, &mut buffer[..length], CONTROL_MS, waiter);
                came.ok().map(|came| Layout::parse(&buffer[..came], heap))
            })
            .flatten();
        let layout = match (own, plan.boot) {
            (Some(Ok(layout)), _) => layout,
            (own, Some(protocol @ (hid::PROTOCOL_KEYBOARD | hid::PROTOCOL_MOUSE))) => {
                if let Some(Err(err)) = own {
                    warn!(
                        "USB: {}: interface {interface}: {err}; using its boot reports",
                        Name(self.bus, path)
                    );
                }
                self.control_out(at, Setup::set_protocol(interface, false))?;
                let boot =
                    if protocol == hid::PROTOCOL_KEYBOARD { BOOT_KEYBOARD } else { BOOT_MOUSE };
                Layout::parse(boot, heap).map_err(|_| Why::Device("out of memory"))?
            }
            (Some(Err(err)), _) => {
                warn!("USB: {}: interface {interface}: {err}", Name(self.bus, path));
                return Ok(());
            }
            (None, _) => return Err(Why::Device("its report descriptor could not be read")),
        };
        let _ = self.control_out(at, Setup::set_idle(interface));
        let what = if layout.is_keyboard() {
            "a keyboard, read into /devices/kbd"
        } else if layout.is_pointer() && layout.is_absolute() {
            "a tablet, read into /devices/mouse"
        } else if layout.is_pointer() {
            "a mouse, read into /devices/mouse"
        } else if layout.has_keys() {
            "media keys, read into /devices/kbd"
        } else {
            "nothing it reports is read"
        };
        info!("USB: {}: interface {interface}: {what}", Name(self.bus, path));
        if !layout.has_keys() && !layout.is_pointer() {
            return Ok(());
        }
        if self.devices[at].hids.len() >= MAX_HIDS || self.devices[at].hids.try_reserve(1).is_err()
        {
            return Err(Why::Device("out of memory"));
        }
        let length = layout.input_bytes().clamp(1, REPORT_MAX);
        let endpoint = Endpoint {
            target,
            number: plan.endpoint & 0xf,
            into_host: true,
            kind: TransferType::Interrupt,
            max_packet: plan.max_packet,
            transactions: plan.transactions,
            interval: plan.interval,
        };
        let pipe = self.listen(endpoint, length)?;
        let hid = Hid {
            interface,
            pipe,
            endpoint: plan.endpoint,
            length,
            lights: layout.has_lights().then_some(u32::MAX),
            layout,
            decoder: Decoder::new(),
            errors: 0,
        };
        let _ = self.devices[at].hids.push_within_capacity(hid);
        self.hid_lights(at);
        Ok(())
    }

    /// The transfer on HID interface `index` of the device at `at` ended:
    /// tells what its report changed, and starts it again.
    #[inline(never)]
    fn hid_report(&mut self, at: usize, index: usize, result: Result<usize, host::Error>) {
        let Some(hid) = self.devices[at].hids.get_mut(index) else { return };
        let (pipe, length, endpoint) = (hid.pipe, hid.length, hid.endpoint);
        let mut errors = hid.errors;
        if let Ok(came) = result {
            let came = came.min(length);
            let Legacy { hcd, devices, buffer, .. } = &mut *self;
            hcd.received(pipe, &mut buffer[..came]);
            let hid = &mut devices[at].hids[index];
            hid.decoder.report(&hid.layout, &buffer[..came], &mut tell);
        }
        let going = self.again(at, pipe, length, result, (&mut errors, endpoint));
        if let Some(hid) = self.devices[at].hids.get_mut(index) {
            hid.errors = errors;
            if !going {
                hid.decoder.release_all(&mut tell);
            }
        }
    }

    /// Sends the keyboards of the device at `at` the lock lights consoled
    /// asked for, if they changed since.
    fn hid_lights(&mut self, at: usize) {
        let leds = keyboard::lights();
        let (target, max) = (self.devices[at].target, self.devices[at].max_packet);
        for index in 0..self.devices[at].hids.len() {
            let hid = &mut self.devices[at].hids[index];
            if hid.lights.is_none_or(|l| l == leds) {
                continue;
            }
            hid.lights = Some(leds);
            let mut report = [0u8; 64];
            let Some((id, len)) = hid.layout.lights_report(leds, &mut report) else { continue };
            let setup = Setup::set_report(hid.interface, hid::REPORT_OUTPUT, id, len as u16);
            let _ = self.control(target, max, setup, &mut report[..len]);
        }
    }
}
