// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hubs (Phase 11a, ADR 0147).
//!
//! Once a hub is configured, [`Controller::start_hub`] reads its hub
//! descriptor, tells the controller its slot is a hub's (with its ports,
//! and a high-speed hub's think time), tells a USB 3 hub how deep it is,
//! powers its ports and waits for their power to be good, then queues
//! every port to be looked at and starts the transfer on its status
//! endpoint. When that transfer finishes, the ports it names are queued
//! and it starts again ([`Controller::hub_status`]).
//!
//! The thread looks at each queued port in turn
//! ([`Controller::hub_port`]): it reads the port's status and clears what
//! changed; a device gone takes every device behind it along; a device
//! come is given 100 ms to settle (USB 2.0's TATTDB), its port is reset
//! (a warm reset for a USB 3 port that is not enabled), and it is set up
//! as a device on a root port is, with its route string, its speed from
//! the port, and, at low or full speed behind a high-speed hub, that
//! hub's slot and port, whose Transaction Translator talks to it.

use radix_klog::{info, warn};
use radix_usb::hub::{HubDescriptor, PortStatus, feature};
use radix_usb::{EndpointDescriptor, Setup, Speed};
use radix_xhci::{Host, HubSlot, Place, code, endpoint_index};

use super::controller::{Controller, Name, Path, RECOVERY_NS, RESET_US, Why, translated};
use crate::sched;

/// How long a hub port's connection is left to settle before its reset
/// (TATTDB, USB 2.0 7.1.7.3).
const DEBOUNCE_NS: u64 = 100_000_000;
/// The least wait after a hub's ports are powered, as Linux waits for a
/// hub that is not a root hub.
const POWER_MS: u16 = 100;
/// How often a hub port is read while it resets.
const RESET_POLL_US: u32 = 10_000;
/// Errors in a row on a hub's status endpoint before it is given up.
const STATUS_ERRORS: u8 = 3;

/// What the thread keeps of a hub.
pub(super) struct Hub {
    /// The ports used: at most 15, which a route string reaches.
    pub(super) ports: u8,
    pub(super) usb3: bool,
    /// Its status endpoint's device context index.
    pub(super) status: u8,
    /// The transfer on it, whose event is the one awaited: an event for
    /// another, stopped or from before, is not.
    pub(super) pending: u64,
    /// The bytes of a change report.
    report: usize,
    /// The ports whose device could not be set up, a bit each, left
    /// alone until it is plugged in again.
    failed: u16,
    /// Errors in a row on its status endpoint.
    errors: u8,
}

impl Controller {
    /// Starts the configured hub in slot `slot`, whose changes `status`
    /// reports.
    #[inline(never)]
    pub(super) fn start_hub(&mut self, slot: u8, status: EndpointDescriptor) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("it is gone"))?;
        let Controller { xhci, host, devices, buffer, bus, .. } = self;
        let attached = &mut devices[at];
        let (path, device) = (attached.path, &mut attached.device);
        let speed = device.place().speed;
        let usb3 = speed.is_super();
        let size = usize::from(HubDescriptor::MAX_SIZE);
        let setup = Setup::hub_descriptor(usb3, HubDescriptor::MAX_SIZE);
        let came = device.control_in(xhci, host, setup, &mut buffer[..size])?;
        let descriptor = HubDescriptor::parse(&buffer[..came], usb3)
            .ok_or(Why::Device("it sent no hub descriptor"))?;
        let think_time = if speed == Speed::High { descriptor.think_time() } else { 0 };
        let slot_says = HubSlot { ports: descriptor.ports, think_time, multi_tt: false };
        device.set_hub(xhci, host, slot_says)?;
        if usb3 {
            let depth = Setup::set_hub_depth(path.depth() as u8);
            device.control_out(xhci, host, depth, &[])?;
        }
        let ports = descriptor.usable_ports();
        for port in 1..=ports {
            let power = Setup::set_port_feature(port, feature::PORT_POWER);
            device.control_out(xhci, host, power, &[])?;
        }
        info!("USB: {}: a hub of {} ports", Name(*bus, path), descriptor.ports);
        if descriptor.ports > ports {
            warn!("USB: {}: only its first {ports} ports are used", Name(*bus, path));
        }
        if path.child(1).is_none() {
            warn!(
                "USB: {}: {} hubs deep; what is plugged into it is not used",
                Name(*bus, path),
                path.depth() + 1
            );
            return Ok(());
        }
        sched::sleep_ns(u64::from(descriptor.power_good_ms.max(POWER_MS)) * 1_000_000);
        let report = descriptor.report_size().min(usize::from(status.max_packet()).max(1));
        attached.hub = Some(Hub {
            ports,
            usb3,
            status: endpoint_index(status.address),
            pending: 0,
            report,
            failed: 0,
            errors: 0,
        });
        for port in 1..=ports {
            self.queue(slot, port);
        }
        self.listen(at)
    }

    /// Starts the transfer on the status endpoint of the hub at `at`.
    fn listen(&mut self, at: usize) -> Result<(), Why> {
        let attached = &mut self.devices[at];
        let Some(hub) = &mut attached.hub else { return Ok(()) };
        hub.pending = attached.device.submit_in(&self.xhci, hub.status, hub.report)?;
        Ok(())
    }

    /// The transfer on the status endpoint of the hub at `at` finished
    /// with `code`, `residual` bytes short: queues the ports it names, and
    /// starts it again.
    #[inline(never)]
    pub(super) fn hub_status(&mut self, at: usize, code: u8, residual: u32) {
        let attached = &mut self.devices[at];
        let (slot, path) = (attached.device.slot(), attached.path);
        let Some(hub) = &mut attached.hub else { return };
        match code {
            code::SUCCESS | code::SHORT_PACKET => {
                hub.errors = 0;
                let came = hub.report.saturating_sub(residual as usize);
                let (status, ports) = (hub.status, hub.ports);
                let mut bits = [0u8; 2];
                let came = came.min(bits.len());
                if attached.device.received(status, &mut bits[..came]).is_err() {
                    return;
                }
                if bits[0] & 1 != 0 {
                    self.hub_changed(slot, path);
                }
                for port in 1..=ports {
                    if bits[usize::from(port / 8)] & (1 << (port % 8)) != 0 {
                        self.queue(slot, port);
                    }
                }
            }
            // Stopped by the driver.
            code::STOPPED => return,
            failed => {
                hub.errors += 1;
                let (status, errors) = (hub.status, hub.errors);
                if errors > STATUS_ERRORS {
                    warn!(
                        "USB: {}: the hub stopped reporting changes: {}",
                        Name(self.bus, path),
                        code::name(failed)
                    );
                    return;
                }
                if attached.device.recover(&mut self.xhci, &mut self.host, status, failed).is_err()
                {
                    return;
                }
            }
        }
        // Its slot is still the hub's: recovering it made no other change.
        if let Some(at) = self.find(slot)
            && let Err(why) = self.listen(at)
        {
            warn!("USB: {}: the hub's changes cannot be read: {why}", Name(self.bus, path));
        }
    }

    /// The hub in slot `slot` says it changed itself: reads what, and
    /// clears it.
    fn hub_changed(&mut self, slot: u8, path: Path) {
        let mut bytes = [0u8; 4];
        if self.hub_in(slot, Setup::hub_status(), &mut bytes).is_err() {
            return;
        }
        let (status, change) =
            (u16::from_le_bytes([bytes[0], bytes[1]]), u16::from_le_bytes([bytes[2], bytes[3]]));
        if change & 1 != 0 {
            let _ = self.hub_out(slot, Setup::clear_hub_feature(feature::C_HUB_LOCAL_POWER));
        }
        if change & 2 != 0 {
            let _ = self.hub_out(slot, Setup::clear_hub_feature(feature::C_HUB_OVER_CURRENT));
            if status & 2 != 0 {
                warn!("USB: {}: the hub draws too much current", Name(self.bus, path));
            }
        }
    }

    /// A control transfer from the hub in slot `slot`.
    fn hub_in(&mut self, slot: u8, setup: Setup, out: &mut [u8]) -> Result<usize, Why> {
        let at = self.find(slot).ok_or(Why::Device("its hub is gone"))?;
        let device = &mut self.devices[at].device;
        Ok(device.control_in(&mut self.xhci, &mut self.host, setup, out)?)
    }

    /// A control transfer to the hub in slot `slot`, without data.
    fn hub_out(&mut self, slot: u8, setup: Setup) -> Result<(), Why> {
        let at = self.find(slot).ok_or(Why::Device("its hub is gone"))?;
        let device = &mut self.devices[at].device;
        Ok(device.control_out(&mut self.xhci, &mut self.host, setup, &[])?)
    }

    /// The status of port `port` of the hub in slot `slot`.
    fn port_status(&mut self, slot: u8, port: u8, usb3: bool) -> Result<PortStatus, Why> {
        let mut bytes = [0u8; PortStatus::SIZE as usize];
        let came = self.hub_in(slot, Setup::port_status(port), &mut bytes)?;
        PortStatus::parse(&bytes[..came], usb3).ok_or(Why::Device("its hub's port status is short"))
    }

    /// Whether the device on port `port` of the hub at `at` could not be
    /// set up.
    fn hub_failed(&self, at: usize, port: u8) -> bool {
        self.devices[at].hub.as_ref().is_some_and(|hub| hub.failed & (1 << port) != 0)
    }

    fn set_hub_failed(&mut self, slot: u8, port: u8, failed: bool) {
        let Some(at) = self.find(slot) else { return };
        if let Some(hub) = &mut self.devices[at].hub {
            if failed {
                hub.failed |= 1 << port;
            } else {
                hub.failed &= !(1 << port);
            }
        }
    }

    /// Port `port` of the hub in slot `slot` may have changed: lets go of
    /// a device that left, and sets up one that came.
    #[inline(never)]
    pub(super) fn hub_port(&mut self, slot: u8, port: u8) {
        let Some(at) = self.find(slot) else { return };
        let Some(usb3) = self.devices[at].hub.as_ref().map(|hub| hub.usb3) else { return };
        let Some(path) = self.devices[at].path.child(port) else { return };
        let Ok(status) = self.port_status(slot, port, usb3) else { return };
        for change in status.changes() {
            let _ = self.hub_out(slot, Setup::clear_port_feature(port, change));
        }
        if status.connect_changed() || !status.connected() {
            self.set_hub_failed(slot, port, false);
            self.detach(path);
        }
        if status.over_current_changed() && status.over_current() {
            warn!("USB: {}: the port draws too much current", Name(self.bus, path));
        }
        let Some(at) = self.find(slot) else { return };
        if status.connected()
            && !self.hub_failed(at, port)
            && !self.devices.iter().any(|d| d.path == path)
        {
            self.attach_behind(slot, port, path, usb3);
        }
    }

    /// Sets up the device on port `port` of the hub in slot `slot`, at
    /// `path`, trying twice.
    fn attach_behind(&mut self, slot: u8, port: u8, path: Path, usb3: bool) {
        sched::sleep_ns(DEBOUNCE_NS);
        for attempt in 0..2 {
            let Err(why) = self.try_attach_behind(slot, port, path, usb3) else { return };
            let connected = self.port_status(slot, port, usb3).is_ok_and(|s| s.connected());
            if attempt == 1 || !connected {
                if connected {
                    warn!("USB: {}: the device could not be set up: {why}", Name(self.bus, path));
                    self.set_hub_failed(slot, port, true);
                }
                return;
            }
        }
    }

    fn try_attach_behind(&mut self, slot: u8, port: u8, path: Path, usb3: bool) -> Result<(), Why> {
        let status = self.hub_reset(slot, port, usb3)?;
        sched::sleep_ns(RECOVERY_NS);
        let speed = status.speed();
        let at = self.find(slot).ok_or(Why::Device("its hub is gone"))?;
        let parent = self.devices[at].device.place();
        // A slow device behind a high-speed hub is reached through that
        // hub's Transaction Translator; behind a slow hub, through the one
        // that hub is behind, if any.
        let (tt_slot, tt_port) = if !translated(speed) {
            (0, 0)
        } else if parent.speed == Speed::High {
            (slot, port)
        } else {
            (parent.tt_slot, parent.tt_port)
        };
        let speed_id = self.xhci.port_protocol(path.root_port).map_or(0, |p| p.speed_id(speed));
        let place = Place {
            root_port: path.root_port,
            route: path.route,
            speed,
            speed_id,
            tt_slot,
            tt_port,
        };
        self.set_up(place)
    }

    /// Resets port `port` of the hub in slot `slot` and waits for it;
    /// returns its status after.
    fn hub_reset(&mut self, slot: u8, port: u8, usb3: bool) -> Result<PortStatus, Why> {
        // A USB 3 port whose link is not up takes a warm reset.
        let before = self.port_status(slot, port, usb3)?;
        let warm = usb3 && !before.enabled();
        let reset = if warm { feature::BH_PORT_RESET } else { feature::PORT_RESET };
        self.hub_out(slot, Setup::set_port_feature(port, reset))?;
        let deadline = self.host.now_us() + RESET_US;
        loop {
            self.host.wait(RESET_POLL_US);
            let status = self.port_status(slot, port, usb3)?;
            if status.reset_changed() && !status.resetting() {
                for change in status.changes() {
                    if matches!(change, feature::C_PORT_RESET | feature::C_BH_PORT_RESET) {
                        self.hub_out(slot, Setup::clear_port_feature(port, change))?;
                    }
                }
                if !status.connected() {
                    return Err(Why::Device("it was unplugged"));
                }
                if !status.enabled() {
                    return Err(Why::Device("its port would not enable"));
                }
                return Ok(status);
            }
            if !status.connected() {
                return Err(Why::Device("it was unplugged"));
            }
            if self.host.now_us() >= deadline {
                return Err(Why::Device("its port did not finish its reset"));
            }
        }
    }
}
