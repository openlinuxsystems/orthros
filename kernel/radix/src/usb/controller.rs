// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A controller's thread.
//!
//! [`run`] takes the controller from the firmware and starts it
//! (`radix_xhci::Xhci::new`), then loops: it reads the controller's
//! events, and for each root port that changed, sets up the device
//! plugged into it or lets go of the one pulled out (and of every device
//! behind it, if it is a hub). Root ports with a device on them when the
//! controller starts raise no event, so each is looked at once first. The
//! thread sleeps until the interrupt wakes it, or for 10 ms when there is
//! none (and at most a second either way).
//!
//! Setting up a device: a USB 2 port is reset, which enables it (a USB 3
//! port enables itself once its link is up, or after a warm reset); after
//! the 10 ms the device has to recover, the device gets a slot and its
//! address; the first eight bytes of its descriptor give endpoint 0's
//! packet size, then the whole descriptor, its strings (in the first
//! language it lists) and its first configuration say what it is, which
//! the log says. The controller is given the endpoints of the
//! configuration's interfaces, and the device is put in it. A device that
//! cannot be set up is tried once more, with another port reset, and then
//! left alone until it is plugged in again. Events that come while the
//! thread waits for a command or a transfer are kept, and handled after.
//!
//! A hub is set up the same way, then driven from [`super::hub`]: the
//! ports it reports changed are queued, and the thread looks at each in
//! turn, so that a hub found behind a hub is never set up from inside
//! another's setup. A device's HID interfaces, its keyboards, mice and
//! tablets, are then read from [`super::hid`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use radix_hal::Devices;
use radix_hw::Dma;
use radix_klog::{TextBuf, error, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_usb::{
    Configuration, DeviceDescriptor, EndpointDescriptor, Setup, Speed, TransferType, class,
    class_name, descriptor, hub,
};
use radix_xhci::{BUFFER_SIZE, Device, Event, Handoff, Host, MAX_ENDPOINTS, Place, Xhci, code};

use super::STARTS;
use super::hid::{self, Hid, HidPlan, MAX_HIDS};
use super::hub::Hub;
use crate::Arch;
use crate::sched;

/// How long the thread sleeps between looks at a controller without an
/// interrupt, and at most with one.
const POLL_NS: u64 = 10_000_000;
const IDLE_NS: u64 = 1_000_000_000;
/// How long a port reset may take: USB 2.0 asks 10 to 20 ms of a hub's
/// port (7.1.7.5); Linux allows 500.
pub(super) const RESET_US: u64 = 500_000;
/// How long a device has to recover after its port's reset (TRSTRCY,
/// USB 2.0 7.1.7.5).
pub(super) const RECOVERY_NS: u64 = 10_000_000;
/// Events kept while the thread waits for another.
const KEPT: usize = 256;
/// Hub ports waiting to be looked at.
const WORK: usize = 256;
/// Root ports: up to 255, a bit each.
const PORT_WORDS: usize = 4;

/// What the driver waits with: the thread's sleep, and the events kept
/// meanwhile.
pub(super) struct KernelHost {
    kept: Vec<Event, Kmalloc>,
    /// Events that did not fit.
    lost: u64,
}

impl Host for KernelHost {
    fn wait(&mut self, us: u32) {
        sched::block_until(sched::now_ns().saturating_add(u64::from(us) * 1000), "usb");
    }

    fn now_us(&mut self) -> u64 {
        sched::now_ns() / 1000
    }

    fn other(&mut self, event: Event) {
        if self.kept.push_within_capacity(event).is_err() {
            self.lost += 1;
        }
    }
}

/// DMA memory for the controller.
pub(super) fn alloc(pages: usize, below_4g: bool) -> Option<Dma> {
    <Arch as Devices>::alloc_dma(pages, below_4g)
}

/// Where a device is: the root port it is on, or its hubs are on, and
/// the route string of hub ports below that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Path {
    pub(super) root_port: u8,
    pub(super) route: u32,
}

impl Path {
    /// How many hubs are between it and its root port.
    pub(super) fn depth(self) -> usize {
        hub::depth(self.route)
    }

    /// Where the device on port `port` of the hub here is.
    pub(super) fn child(self, port: u8) -> Option<Path> {
        Some(Path { root_port: self.root_port, route: hub::child_route(self.route, port)? })
    }

    /// Whether it is below the hub at `hub`.
    pub(super) fn is_below(self, hub: Path) -> bool {
        self.root_port == hub.root_port && hub::is_below(self.route, hub.route)
    }
}

/// A device's name in the log, as Linux names it: its bus, its root port
/// and its hub ports, `1-3.2`.
pub(super) struct Name(pub(super) usize, pub(super) Path);

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Name(bus, path) = self;
        write!(f, "{bus}-{}", path.root_port)?;
        for level in 0..path.depth() {
            write!(f, ".{}", (path.route >> (4 * level)) & 0xf)?;
        }
        Ok(())
    }
}

/// A device the thread has set up.
pub(super) struct Attached {
    pub(super) path: Path,
    /// On the heap: its endpoints make it big.
    pub(super) device: Box<Device, Kmalloc>,
    /// What it calls itself, for the log.
    pub(super) name: TextBuf<64>,
    /// What the thread keeps of it if it is a hub.
    pub(super) hub: Option<Hub>,
    /// Its HID interfaces that are read.
    pub(super) hids: Vec<Hid, Kmalloc>,
}

/// A controller, as its thread runs it.
pub(super) struct Controller {
    pub(super) bus: usize,
    pub(super) xhci: Box<Xhci, Kmalloc>,
    pub(super) host: KernelHost,
    pub(super) devices: Vec<Attached, Kmalloc>,
    /// The root ports whose device could not be set up, left alone until
    /// it is plugged in again.
    failed: [u64; PORT_WORDS],
    /// Where control transfers' data lands.
    pub(super) buffer: Vec<u8, Kmalloc>,
    /// Hub ports to look at: the hub's slot and the port.
    pub(super) work: Vec<(u8, u8), Kmalloc>,
    /// The keyboards' lock lights when the thread last lit them.
    lights: u32,
}

/// Why a device could not be set up.
pub(super) enum Why {
    Driver(radix_xhci::Error),
    Device(&'static str),
}

impl From<radix_xhci::Error> for Why {
    fn from(err: radix_xhci::Error) -> Why {
        Why::Driver(err)
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Driver(err) => write!(f, "{err}"),
            Why::Device(why) => f.write_str(why),
        }
    }
}

/// What a device's first configuration holds, for setting it up.
struct Plan {
    /// bConfigurationValue.
    value: u8,
    /// The endpoints of its interfaces' first settings, the first
    /// `count`.
    endpoints: [EndpointDescriptor; MAX_ENDPOINTS],
    count: usize,
    /// A hub's endpoint that reports its changes.
    hub: Option<EndpointDescriptor>,
    /// Its HID interfaces, the first `hid_count`.
    hids: [HidPlan; MAX_HIDS],
    hid_count: usize,
}

/// The thread of controller `index`.
pub fn run(index: usize) {
    let Some(start) = STARTS.get(index).and_then(radix_sync::Once::get) else { return };
    let Some(mut controller) = Controller::start(index + 1, start) else { return };
    let ports = controller.xhci.info().ports;
    for port in 1..=ports {
        controller.port_changed(port);
    }
    let sleep = if start.interrupt.is_some() { IDLE_NS } else { POLL_NS };
    loop {
        let busy = controller.events() | controller.work() | controller.lights();
        if controller.xhci.failed() {
            error!(
                "USB: bus {}: the controller stopped with an error; its devices are gone",
                controller.bus
            );
            return;
        }
        if !busy {
            sched::block_until(sched::now_ns().saturating_add(sleep), "usb");
        }
    }
}

/// Logs what the controller is.
#[inline(never)]
fn describe(bus: usize, start: &super::Start, xhci: &Xhci) {
    let info = xhci.info();
    // The protocols' port ranges, in the ports' order.
    let mut protocols = TextBuf::<96>::new();
    let ranges =
        (1..=info.ports).filter_map(|port| xhci.protocols().iter().find(|p| p.first_port == port));
    for (i, protocol) in ranges.enumerate() {
        let last = u16::from(protocol.first_port) + u16::from(protocol.ports).max(1) - 1;
        let _ = write!(
            protocols,
            "{}USB {} on {}",
            if i == 0 { "" } else { ", " },
            protocol.major,
            protocol.first_port
        );
        if last > u16::from(protocol.first_port) {
            let _ = write!(protocols, "-{last}");
        }
    }
    // The version in BCD: 0x0100 is 1.0, 0x0096 is 0.96.
    let low = info.version & 0xff;
    let mut version = TextBuf::<8>::new();
    let _ = if low & 0xf == 0 {
        write!(version, "{:x}.{:x}", info.version >> 8, low >> 4)
    } else {
        write!(version, "{:x}.{:02x}", info.version >> 8, low)
    };
    let mut how = TextBuf::<24>::new();
    let _ = match start.interrupt {
        Some((vector, kind)) => write!(how, "{kind} vector {vector:#x}"),
        None => how.write_str("polled"),
    };
    let handoff = match info.handoff {
        Handoff::Given => ", taken from the firmware",
        Handoff::Taken => ", taken from the firmware, which would not let go",
        Handoff::None | Handoff::Free => "",
    };
    info!(
        "USB: bus {bus} is the xHCI {version} controller at {}: {} ports ({protocols}), {} device slots, {how}{handoff}",
        start.address, info.ports, info.slots
    );
}

impl Controller {
    /// Takes the controller from the firmware and starts it. Kept out of
    /// [`run`]'s frame, which the driver's state would not fit in.
    #[inline(never)]
    fn start(bus: usize, start: &super::Start) -> Option<Box<Controller, Kmalloc>> {
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let mut host = KernelHost { kept: Vec::new_in(heap), lost: 0 };
        let mut buffer = Vec::new_in(heap);
        let mut work = Vec::new_in(heap);
        if host.kept.try_reserve_exact(KEPT).is_err()
            || buffer.try_reserve_exact(BUFFER_SIZE).is_err()
            || work.try_reserve_exact(WORK).is_err()
        {
            error!("USB: out of memory for the controller at {}", start.address);
            return None;
        }
        while buffer.push_within_capacity(0).is_ok() {}
        let xhci = match Xhci::new(start.regs.clone(), &mut host, &mut alloc) {
            Ok(xhci) => xhci,
            Err(err) => {
                warn!("USB: the xHCI controller at {} could not be started: {err}", start.address);
                return None;
            }
        };
        describe(bus, start, &xhci);
        let Ok(xhci) = Box::try_new_in(xhci, heap) else {
            error!("USB: out of memory for the controller at {}", start.address);
            return None;
        };
        let controller = Controller {
            bus,
            xhci,
            host,
            devices: Vec::new_in(heap),
            failed: [0; PORT_WORDS],
            buffer,
            work,
            lights: crate::fs::keyboard::lights(),
        };
        Box::try_new_in(controller, heap).ok()
    }

    /// Handles the events that came, and those kept meanwhile; says
    /// whether there were any.
    fn events(&mut self) -> bool {
        let mut any = false;
        loop {
            let event = if self.host.kept.is_empty() {
                match self.xhci.next_event() {
                    Some(event) => event,
                    None => break,
                }
            } else {
                self.host.kept.remove(0)
            };
            any = true;
            match event {
                Event::Port { port } => self.port_changed(port),
                Event::Transfer { trb, slot, endpoint, code, residual } => {
                    self.transfer(trb, slot, endpoint, code, residual);
                }
                Event::Controller { code } => {
                    warn!("USB: bus {}: the controller says: {}", self.bus, code::name(code));
                }
                _ => {}
            }
        }
        self.xhci.events_read();
        if self.host.lost > 0 {
            warn!(
                "USB: bus {}: {} events were lost; looking at every port",
                self.bus, self.host.lost
            );
            self.host.lost = 0;
            for port in 1..=self.xhci.info().ports {
                self.port_changed(port);
            }
            for at in 0..self.devices.len() {
                let attached = &self.devices[at];
                if let Some(hub) = &attached.hub {
                    let (slot, ports) = (attached.device.slot(), hub.ports);
                    for port in 1..=ports {
                        self.queue(slot, port);
                    }
                }
            }
        }
        any
    }

    /// Looks at the hub ports queued; says whether there were any.
    fn work(&mut self) -> bool {
        if self.work.is_empty() {
            return false;
        }
        while !self.work.is_empty() {
            let (slot, port) = self.work.remove(0);
            self.hub_port(slot, port);
        }
        true
    }

    /// Lights every keyboard's lock lights as consoled(8) last asked, if
    /// that changed; says whether it did.
    fn lights(&mut self) -> bool {
        let lights = crate::fs::keyboard::lights();
        if lights == self.lights {
            return false;
        }
        self.lights = lights;
        for at in 0..self.devices.len() {
            self.hid_lights(at);
        }
        true
    }

    /// Queues port `port` of the hub in slot `slot` to be looked at.
    pub(super) fn queue(&mut self, slot: u8, port: u8) {
        if !self.work.contains(&(slot, port))
            && self.work.push_within_capacity((slot, port)).is_err()
        {
            warn!("USB: bus {}: too many hub ports to look at; one is skipped", self.bus);
        }
    }

    /// Where the device in slot `slot` is in [`Controller::devices`].
    pub(super) fn find(&self, slot: u8) -> Option<usize> {
        self.devices.iter().position(|d| d.device.slot() == slot)
    }

    /// The transfer at `trb` on an endpoint other than 0 finished.
    fn transfer(&mut self, trb: u64, slot: u8, endpoint: u8, code: u8, residual: u32) {
        let Some(at) = self.find(slot) else { return };
        let attached = &self.devices[at];
        if attached.hub.as_ref().is_some_and(|hub| hub.status == endpoint && hub.pending == trb) {
            self.hub_status(at, code, residual);
        } else if let Some(index) =
            attached.hids.iter().position(|h| h.endpoint == endpoint && h.pending == trb)
        {
            self.hid_report(at, index, code, residual);
        }
    }

    fn failed(&self, port: u8) -> bool {
        self.failed[usize::from(port) / 64] & (1 << (port % 64)) != 0
    }

    fn set_failed(&mut self, port: u8, failed: bool) {
        let (word, bit) = (usize::from(port) / 64, 1u64 << (port % 64));
        if failed {
            self.failed[word] |= bit;
        } else {
            self.failed[word] &= !bit;
        }
    }

    /// Lets go of the device at `path` and of every device below it, as
    /// they were unplugged: the hub first, as Linux says it.
    pub(super) fn detach(&mut self, path: Path) {
        let bus = self.bus;
        while let Some(at) =
            self.devices.iter().position(|d| d.path == path || d.path.is_below(path))
        {
            let mut gone = self.devices.remove(at);
            hid::gone(&mut gone.hids);
            let slot = gone.device.slot();
            info!("USB: {}: {} was unplugged", Name(bus, gone.path), gone.name);
            self.give_back(gone.device, gone.path);
            // A hub's queued ports are not looked at: its slot may be
            // given to another device.
            self.work.retain(|&(hub, _)| hub != slot);
        }
    }

    /// Gives the slot of the device at `path` back. Kept out of its
    /// callers' frames, which would hold the device.
    #[inline(never)]
    fn give_back(&mut self, device: Box<Device, Kmalloc>, path: Path) {
        if let Err(err) = (*device).disable(&mut self.xhci, &mut self.host) {
            warn!("USB: {}: its slot could not be given back: {err}", Name(self.bus, path));
        }
    }

    /// Root port `port` may have changed: lets go of a device that left,
    /// and sets up one that came.
    #[inline(never)]
    fn port_changed(&mut self, port: u8) {
        let status = self.xhci.port_status(port);
        self.xhci.acknowledge_port(port, status);
        let path = Path { root_port: port, route: 0 };
        if status.connect_changed() || !status.connected() {
            self.set_failed(port, false);
            self.detach(path);
        }
        if status.changes() & radix_xhci::portsc::OVER_CURRENT_CHANGE != 0 && status.over_current()
        {
            warn!("USB: {}: the port draws too much current", Name(self.bus, path));
        }
        if status.connected() && !self.failed(port) && !self.devices.iter().any(|d| d.path == path)
        {
            self.attach(port);
        }
    }

    /// Sets up the device on root port `port`, trying twice.
    fn attach(&mut self, port: u8) {
        for attempt in 0..2 {
            match self.try_attach(port, attempt > 0) {
                Ok(()) => return,
                Err(why) => {
                    if attempt == 1 || !self.xhci.port_status(port).connected() {
                        let path = Path { root_port: port, route: 0 };
                        warn!(
                            "USB: {}: the device could not be set up: {why}",
                            Name(self.bus, path)
                        );
                        self.set_failed(port, true);
                        return;
                    }
                }
            }
        }
    }

    /// Resets root port `port` (warm for a USB 3 one) and waits for it.
    fn reset_port(&mut self, port: u8, warm: bool) -> Result<(), Why> {
        self.xhci.reset_port(port, warm);
        let deadline = self.host.now_us() + RESET_US;
        loop {
            let status = self.xhci.port_status(port);
            if status.reset_changed() && !status.resetting() {
                self.xhci.acknowledge_port(port, status);
                return Ok(());
            }
            if !status.connected() {
                return Err(Why::Device("it was unplugged"));
            }
            if self.host.now_us() >= deadline {
                return Err(Why::Device("its port did not finish its reset"));
            }
            self.host.wait(1_000);
        }
    }

    fn try_attach(&mut self, port: u8, again: bool) -> Result<(), Why> {
        let usb3 = self.xhci.port_protocol(port).is_some_and(|p| p.major == 3);
        if again || !self.xhci.port_status(port).enabled() {
            self.reset_port(port, usb3)?;
            sched::sleep_ns(RECOVERY_NS);
        }
        let status = self.xhci.port_status(port);
        if !status.enabled() {
            return Err(Why::Device("its port would not enable"));
        }
        let speed = self
            .xhci
            .port_speed(port, status)
            .ok_or(Why::Device("its port says a speed it does not name"))?;
        self.set_up(Place::root(port, speed, status.speed_id()))
    }

    /// Gives the device at `place`, its port reset, a slot and an
    /// address, reads what it is, and configures it; a hub is then
    /// started.
    #[inline(never)]
    pub(super) fn set_up(&mut self, place: Place) -> Result<(), Why> {
        let path = Path { root_port: place.root_port, route: place.route };
        // The memory first, so that a device given a slot always has a
        // home, and a place in the list.
        let heap = HEAP.with(Flags::MAY_SLEEP);
        let (Ok(home), Ok(())) = (Box::try_new_uninit_in(heap), self.devices.try_reserve(1)) else {
            return Err(Why::Device("out of memory"));
        };
        let mut device =
            Box::write(home, Device::address(&mut self.xhci, &mut self.host, &mut alloc, place)?);
        let mut name = TextBuf::<64>::new();
        let result = self.identify(&mut device, path, &mut name).and_then(|plan| {
            let endpoints = &plan.endpoints[..plan.count];
            device.configure(&mut self.xhci, &mut self.host, &mut alloc, plan.value, endpoints)?;
            Ok(plan)
        });
        let plan = match result {
            Ok(plan) => plan,
            Err(why) => {
                self.give_back(device, path);
                return Err(why);
            }
        };
        let slot = device.slot();
        let attached = Attached { path, device, name, hub: None, hids: hid::none() };
        let _ = self.devices.push_within_capacity(attached);
        if let Some(status) = plan.hub
            && let Err(why) = self.start_hub(slot, status)
        {
            warn!("USB: {}: the hub could not be started: {why}", Name(self.bus, path));
        }
        for &interface in &plan.hids[..plan.hid_count] {
            if let Err(why) = self.start_hid(slot, interface) {
                warn!(
                    "USB: {}: interface {} could not be read: {why}",
                    Name(self.bus, path),
                    interface.interface
                );
            }
        }
        Ok(())
    }

    /// Reads what the device is, gives the controller its packet size, and
    /// logs it; its name goes in `name`. Returns what its first
    /// configuration holds.
    #[inline(never)]
    fn identify(
        &mut self,
        device: &mut Device,
        path: Path,
        name: &mut TextBuf<64>,
    ) -> Result<Plan, Why> {
        let speed = device.place().speed;
        let Controller { xhci, host, buffer, bus, .. } = self;
        let mut bytes = [0u8; radix_usb::DEVICE_DESCRIPTOR_SIZE];
        let first = Setup::get_descriptor(descriptor::DEVICE, 0, 0, 8);
        device.control_in(xhci, host, first, &mut bytes[..8])?;
        let max = DeviceDescriptor::parse(&bytes[..8])
            .and_then(|d| d.max_packet0(speed))
            .ok_or(Why::Device("it says a packet size its speed does not allow"))?;
        device.set_max_packet(xhci, host, max)?;
        let whole = Setup::get_descriptor(descriptor::DEVICE, 0, 0, bytes.len() as u16);
        let came = device.control_in(xhci, host, whole, &mut bytes)?;
        let found = DeviceDescriptor::parse(&bytes[..came])
            .ok_or(Why::Device("it sent no device descriptor"))?;
        // Strings, if it has them; a device may refuse them.
        let language = {
            let setup = Setup::get_descriptor(descriptor::STRING, 0, 0, 255);
            match device.control_in(xhci, host, setup, &mut buffer[..255]) {
                Ok(came) => radix_usb::first_language(&buffer[..came]),
                Err(_) => 0x0409,
            }
        };
        let mut string = |index: u8, out: &mut dyn Write| {
            if index == 0 {
                return;
            }
            let setup = Setup::get_descriptor(descriptor::STRING, index, language, 255);
            if let Ok(came) = device.control_in(xhci, host, setup, &mut buffer[..255]) {
                let _ = radix_usb::string(&buffer[..came], out);
            }
        };
        string(found.product_string, name);
        let mut maker = TextBuf::<64>::new();
        string(found.manufacturer_string, &mut maker);
        if name.is_empty() {
            let _ = name.write_str("a USB device");
        }
        // Its first configuration: what its interfaces are, and their
        // endpoints.
        let header = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, 9);
        let total = device
            .control_in(xhci, host, header, &mut buffer[..9])
            .ok()
            .and_then(|came| Configuration::parse(&buffer[..came]))
            .map_or(0, |config| usize::from(config.total_length).min(BUFFER_SIZE));
        if total < 9 {
            return Err(Why::Device("it has no configuration"));
        }
        let all = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, total as u16);
        let came = device.control_in(xhci, host, all, &mut buffer[..total])?;
        let config = Configuration::parse(&buffer[..came])
            .ok_or(Why::Device("its configuration is broken"))?;
        let mut plan = Plan {
            value: config.value,
            endpoints: [EndpointDescriptor::default(); MAX_ENDPOINTS],
            count: 0,
            hub: None,
            hids: [HidPlan::default(); MAX_HIDS],
            hid_count: 0,
        };
        let mut named: [&str; 4] = [""; 4];
        let mut count = 0;
        let mut endpoints = 0;
        for interface in config.interfaces().filter(|i| i.descriptor.alternate == 0) {
            let d = interface.descriptor;
            let this = class_name(d.class, d.subclass, d.protocol);
            if count < named.len() && !named[..count].contains(&this) {
                named[count] = this;
                count += 1;
            }
            if d.class == class::HID && plan.hid_count < MAX_HIDS {
                let descriptor = interface
                    .descriptors()
                    .find(|&(kind, _)| kind == descriptor::HID)
                    .map(|(_, bytes)| bytes);
                let input = interface
                    .endpoints()
                    .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)
                    .map(|e| e.address);
                if let Some(hid) = HidPlan::of(d, descriptor, input) {
                    plan.hids[plan.hid_count] = hid;
                    plan.hid_count += 1;
                }
            }
            for endpoint in interface.endpoints() {
                if found.class == class::HUB
                    && d.class == class::HUB
                    && endpoint.is_in()
                    && plan.hub.is_none()
                {
                    plan.hub = Some(endpoint);
                }
                if let Some(slot) = plan.endpoints.get_mut(plan.count) {
                    *slot = endpoint;
                    plan.count += 1;
                }
                endpoints += 1;
            }
        }
        let mut kind = TextBuf::<64>::new();
        if found.class != class::PER_INTERFACE {
            let _ = kind.write_str(class_name(found.class, found.subclass, found.protocol));
        } else {
            for (i, this) in named[..count].iter().enumerate() {
                let _ = write!(kind, "{}{this}", if i == 0 { "" } else { " and " });
            }
            if count == 0 {
                let _ = kind.write_str("composite");
            }
        }
        let mut from = TextBuf::<72>::new();
        if !maker.is_empty() {
            let _ = write!(from, ", {maker}");
        }
        info!(
            "USB: {}: {name} ({:04x}:{:04x}{from}), {kind}, {speed}",
            Name(*bus, path),
            found.vendor,
            found.product
        );
        if endpoints > plan.count {
            warn!(
                "USB: {}: only {} of its {endpoints} endpoints are used",
                Name(*bus, path),
                plan.count
            );
        }
        Ok(plan)
    }
}

/// The speeds below high speed, which a high-speed hub's Transaction
/// Translator carries.
pub(super) fn translated(speed: Speed) -> bool {
    matches!(speed, Speed::Low | Speed::Full)
}
