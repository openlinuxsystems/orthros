// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A controller's thread.
//!
//! [`run`] takes the controller from the firmware and starts it
//! (`radix_xhci::Xhci::new`), then loops: it reads the controller's
//! events, and for each root port that changed, sets up the device
//! plugged into it or lets go of the one pulled out. Root ports with a
//! device on them when the controller starts raise no event, so each is
//! looked at once first. The thread sleeps until the interrupt wakes it,
//! or for 10 ms when there is none (and at most a second either way).
//!
//! Setting up a device: a USB 2 port is reset, which enables it (a USB 3
//! port enables itself once its link is up, or after a warm reset); after
//! the 10 ms the device has to recover, the device gets a slot and its
//! address; the first eight bytes of its descriptor give endpoint 0's
//! packet size, then the whole descriptor, its strings (in the first
//! language it lists) and its first configuration say what it is, which
//! the log says. A device that cannot be set up is tried once more, with
//! another port reset, and then left alone until it is plugged in again.
//! Events that come while the thread waits for a command or a transfer
//! are kept, and handled after.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;

use radix_hal::Devices;
use radix_hw::Dma;
use radix_klog::{TextBuf, error, info, warn};
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_usb::{Configuration, DeviceDescriptor, Setup, Speed, class, class_name, descriptor};
use radix_xhci::{BUFFER_SIZE, Device, Event, Handoff, Host, Place, Xhci, code};

use super::STARTS;
use crate::Arch;
use crate::sched;

/// How long the thread sleeps between looks at a controller without an
/// interrupt, and at most with one.
const POLL_NS: u64 = 10_000_000;
const IDLE_NS: u64 = 1_000_000_000;
/// How long a port reset may take: USB 2.0 asks 10 to 20 ms of a hub's
/// port (7.1.7.5); Linux allows 500.
const RESET_US: u64 = 500_000;
/// How long a device has to recover after its port's reset (TRSTRCY,
/// USB 2.0 7.1.7.5).
const RECOVERY_NS: u64 = 10_000_000;
/// Events kept while the thread waits for another.
const KEPT: usize = 256;
/// Root ports: up to 255, a bit each.
const PORT_WORDS: usize = 4;

/// What the driver waits with: the thread's sleep, and the events kept
/// meanwhile.
struct KernelHost {
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
fn alloc(pages: usize, below_4g: bool) -> Option<Dma> {
    <Arch as Devices>::alloc_dma(pages, below_4g)
}

/// A device on a root port.
struct Attached {
    port: u8,
    device: Device,
    /// What it calls itself, for the log.
    name: TextBuf<64>,
}

/// A controller, as its thread runs it.
struct Controller {
    bus: usize,
    xhci: Box<Xhci, Kmalloc>,
    host: KernelHost,
    devices: Vec<Attached, Kmalloc>,
    /// The ports whose device could not be set up, left alone until it is
    /// plugged in again.
    failed: [u64; PORT_WORDS],
    /// Where control transfers' data lands.
    buffer: Vec<u8, Kmalloc>,
}

/// Why a device could not be set up.
enum Why {
    Driver(radix_xhci::Error),
    Device(&'static str),
}

impl From<radix_xhci::Error> for Why {
    fn from(err: radix_xhci::Error) -> Why {
        Why::Driver(err)
    }
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Driver(err) => write!(f, "{err}"),
            Why::Device(why) => f.write_str(why),
        }
    }
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
        controller.events();
        if controller.xhci.failed() {
            error!(
                "USB: bus {}: the controller stopped with an error; its devices are gone",
                controller.bus
            );
            return;
        }
        sched::block_until(sched::now_ns().saturating_add(sleep), "usb");
    }
}

/// Logs what the controller is.
#[inline(never)]
fn describe(bus: usize, start: &super::Start, xhci: &Xhci) {
    let info = xhci.info();
    let mut protocols = TextBuf::<96>::new();
    for (i, protocol) in xhci.protocols().iter().enumerate() {
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
        if host.kept.try_reserve_exact(KEPT).is_err()
            || buffer.try_reserve_exact(BUFFER_SIZE).is_err()
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
        };
        Box::try_new_in(controller, heap).ok()
    }

    /// Handles the events that came, and those kept meanwhile.
    fn events(&mut self) {
        loop {
            let event = if self.host.kept.is_empty() {
                match self.xhci.next_event() {
                    Some(event) => event,
                    None => break,
                }
            } else {
                self.host.kept.remove(0)
            };
            match event {
                Event::Port { port } => self.port_changed(port),
                Event::Controller { code } => {
                    warn!("USB: bus {}: the controller says: {}", self.bus, code::name(code));
                }
                _ => {}
            }
        }
        self.xhci.events_read();
        if self.host.lost > 0 {
            warn!("USB: bus {}: {} events were lost; looking at every port", self.bus, self.host.lost);
            self.host.lost = 0;
            for port in 1..=self.xhci.info().ports {
                self.port_changed(port);
            }
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

    /// Root port `port` may have changed: lets go of a device that left,
    /// and sets up one that came.
    #[inline(never)]
    fn port_changed(&mut self, port: u8) {
        let status = self.xhci.port_status(port);
        self.xhci.acknowledge_port(port, status);
        let bus = self.bus;
        if status.connect_changed() || !status.connected() {
            self.set_failed(port, false);
            if let Some(at) = self.devices.iter().position(|d| d.port == port) {
                let gone = self.devices.remove(at);
                info!("USB: {bus}-{port}: {} was unplugged", gone.name);
                if let Err(err) = gone.device.disable(&mut self.xhci, &mut self.host) {
                    warn!("USB: {bus}-{port}: its slot could not be given back: {err}");
                }
            }
        }
        if status.changes() & radix_xhci::portsc::OVER_CURRENT_CHANGE != 0 && status.over_current() {
            warn!("USB: {bus}-{port}: the port draws too much current");
        }
        if status.connected()
            && !self.failed(port)
            && !self.devices.iter().any(|d| d.port == port)
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
                        warn!("USB: {}-{port}: the device could not be set up: {why}", self.bus);
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
        let place = Place { root_port: port, route: 0, speed, speed_id: status.speed_id() };
        let mut device = Device::address(&mut self.xhci, &mut self.host, &mut alloc, place)?;
        let mut name = TextBuf::<64>::new();
        match self.identify(&mut device, speed, &mut name, port) {
            Ok(()) => {
                if self.devices.try_reserve(1).is_err() {
                    let _ = device.disable(&mut self.xhci, &mut self.host);
                    return Err(Why::Device("out of memory"));
                }
                let _ = self.devices.push_within_capacity(Attached { port, device, name });
                Ok(())
            }
            Err(why) => {
                let _ = device.disable(&mut self.xhci, &mut self.host);
                Err(why)
            }
        }
    }

    /// Reads what the device is, gives the controller its packet size, and
    /// logs it; its name goes in `name`.
    fn identify(
        &mut self,
        device: &mut Device,
        speed: Speed,
        name: &mut TextBuf<64>,
        port: u8,
    ) -> Result<(), Why> {
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
        // What it is: its class, or its interfaces' classes.
        let mut kind = TextBuf::<64>::new();
        if found.class != class::PER_INTERFACE {
            let _ = kind.write_str(class_name(found.class, found.subclass, found.protocol));
        } else {
            let header = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, 9);
            let total = device
                .control_in(xhci, host, header, &mut buffer[..9])
                .ok()
                .and_then(|came| Configuration::parse(&buffer[..came]))
                .map_or(0, |config| usize::from(config.total_length).min(BUFFER_SIZE));
            let all = Setup::get_descriptor(descriptor::CONFIGURATION, 0, 0, total as u16);
            let config = if total >= 9 {
                device
                    .control_in(xhci, host, all, &mut buffer[..total])
                    .ok()
                    .and_then(|came| Configuration::parse(&buffer[..came]))
            } else {
                None
            };
            let mut named: [&str; 4] = [""; 4];
            let mut count = 0;
            for interface in config.into_iter().flat_map(|c| c.interfaces()) {
                let d = interface.descriptor;
                let this = class_name(d.class, d.subclass, d.protocol);
                if count < named.len() && !named[..count].contains(&this) {
                    named[count] = this;
                    count += 1;
                }
            }
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
            "USB: {bus}-{port}: {name} ({:04x}:{:04x}{from}), {kind}, {speed}",
            found.vendor, found.product
        );
        Ok(())
    }
}
