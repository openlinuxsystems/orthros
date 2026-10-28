// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hubs, as chapter 11 of USB 2.0 and chapter 10 of USB 3.2 describe them.
//!
//! A hub is a device of class 9 with one interrupt IN endpoint, on which
//! it reports which of its ports changed: a bitmap, bit 0 for the hub
//! itself and bit `n` for port `n`. The host learns how many ports it has
//! from its hub descriptor ([`HubDescriptor`]), powers them, and drives
//! each with class requests to the port (recipient "other", wIndex the
//! port): [`Setup::port_status`] reads its status and what changed
//! ([`PortStatus`]), [`Setup::set_port_feature`] powers or resets it, and
//! [`Setup::clear_port_feature`] acknowledges a change. A USB 3 hub has
//! its own descriptor and status layout, and is told how deep in the tree
//! it is ([`Setup::set_hub_depth`]), which says where its devices' route
//! string nibble goes.

use crate::{CLASS, DEVICE_TO_HOST, Setup, Speed, TO_DEVICE, TO_OTHER, descriptor, request};

/// SET_HUB_DEPTH, a USB 3 hub's request (USB 3.2, table 10-8).
pub const SET_HUB_DEPTH: u8 = 12;

/// Port feature selectors (USB 2.0, table 11-17; USB 3.2, table 10-9),
/// and the hub's own two.
pub mod feature {
    /// The hub's local power supply changed.
    pub const C_HUB_LOCAL_POWER: u16 = 0;
    /// The hub's over-current changed.
    pub const C_HUB_OVER_CURRENT: u16 = 1;
    /// The port's enable (clearing it disables the port).
    pub const PORT_ENABLE: u16 = 1;
    /// Suspends the port (USB 2).
    pub const PORT_SUSPEND: u16 = 2;
    /// Resets the port: a USB 2 reset, or a USB 3 hot reset.
    pub const PORT_RESET: u16 = 4;
    /// The port's power.
    pub const PORT_POWER: u16 = 8;
    /// Connect status changed.
    pub const C_PORT_CONNECTION: u16 = 16;
    /// The port was disabled by an error (USB 2).
    pub const C_PORT_ENABLE: u16 = 17;
    /// The port finished resuming (USB 2).
    pub const C_PORT_SUSPEND: u16 = 18;
    /// Over-current changed.
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    /// The port finished its reset.
    pub const C_PORT_RESET: u16 = 20;
    /// The link changed state (USB 3).
    pub const C_PORT_LINK_STATE: u16 = 25;
    /// The port could not be configured (USB 3).
    pub const C_PORT_CONFIG_ERROR: u16 = 26;
    /// A warm reset, which retrains the link (USB 3).
    pub const BH_PORT_RESET: u16 = 28;
    /// The port finished its warm reset (USB 3).
    pub const C_BH_PORT_RESET: u16 = 29;
}

/// The most ports a hub has that the driver uses: an xHCI route string
/// names a hub's port in four bits, so ports past 15 cannot be reached.
pub const MAX_PORTS: u8 = 15;

/// The most hubs between a root port and a device (USB 2.0, 4.1.1): a
/// route string holds five ports.
pub const MAX_DEPTH: usize = 5;

/// What a hub's descriptor says (USB 2.0, 11.23.2.1; USB 3.2, 10.15.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDescriptor {
    /// bNbrPorts: how many downstream ports it has.
    pub ports: u8,
    /// wHubCharacteristics.
    pub characteristics: u16,
    /// How long a port takes to have good power after it is powered on, in
    /// milliseconds (bPwrOn2PwrGood, in units of 2 ms).
    pub power_good_ms: u16,
}

impl HubDescriptor {
    /// The bytes to ask for: enough for every field and the bitmaps of up
    /// to 255 ports.
    pub const MAX_SIZE: u16 = 71;

    /// Reads a USB 2 hub's descriptor (type 0x29) or, with `usb3`, a USB 3
    /// hub's (type 0x2a), from what the hub sent.
    pub fn parse(bytes: &[u8], usb3: bool) -> Option<HubDescriptor> {
        let kind = if usb3 { descriptor::SUPERSPEED_HUB } else { descriptor::HUB };
        if bytes.len() < 7 || usize::from(bytes[0]) < 7 || bytes[1] != kind {
            return None;
        }
        Some(HubDescriptor {
            ports: bytes[2],
            characteristics: u16::from_le_bytes([bytes[3], bytes[4]]),
            power_good_ms: u16::from(bytes[5]) * 2,
        })
    }

    /// The ports the driver uses: the first [`MAX_PORTS`].
    pub fn usable_ports(&self) -> u8 {
        self.ports.min(MAX_PORTS)
    }

    /// A high-speed hub's Transaction Translator think time, as the xHCI
    /// slot context takes it: 0 to 3 for 8 to 32 full-speed bit times.
    pub fn think_time(&self) -> u8 {
        ((self.characteristics >> 5) & 3) as u8
    }

    /// The bytes of a change report: a bit for the hub and one per port.
    pub fn report_size(&self) -> usize {
        usize::from(self.ports) / 8 + 1
    }
}

/// A port's status and what changed, as GET_STATUS to the port answers
/// (USB 2.0, 11.24.2.7; USB 3.2, 10.16.2.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    /// wPortStatus.
    pub status: u16,
    /// wPortChange.
    pub change: u16,
    /// Whether it is a USB 3 hub's port, whose bits differ.
    pub usb3: bool,
}

impl PortStatus {
    /// The bytes GET_STATUS returns.
    pub const SIZE: u16 = 4;

    /// Reads what GET_STATUS returned for a port of a USB 2 hub, or of a
    /// USB 3 one with `usb3`.
    pub fn parse(bytes: &[u8], usb3: bool) -> Option<PortStatus> {
        let [s0, s1, c0, c1, ..] = *bytes else { return None };
        Some(PortStatus {
            status: u16::from_le_bytes([s0, s1]),
            change: u16::from_le_bytes([c0, c1]),
            usb3,
        })
    }

    /// A device is connected.
    pub fn connected(&self) -> bool {
        self.status & 1 != 0
    }

    /// The port is enabled: reset and passing traffic.
    pub fn enabled(&self) -> bool {
        self.status & 1 << 1 != 0
    }

    /// The port draws too much current.
    pub fn over_current(&self) -> bool {
        self.status & 1 << 3 != 0
    }

    /// The port is resetting.
    pub fn resetting(&self) -> bool {
        self.status & 1 << 4 != 0
    }

    /// The port is powered: bit 8 on a USB 2 hub, 9 on a USB 3 one.
    pub fn powered(&self) -> bool {
        self.status & if self.usb3 { 1 << 9 } else { 1 << 8 } != 0
    }

    /// The speed of the device on it: a USB 3 hub's ports are SuperSpeed;
    /// a USB 2 hub's say low or high speed, and full speed otherwise.
    pub fn speed(&self) -> Speed {
        if self.usb3 {
            Speed::Super
        } else if self.status & 1 << 9 != 0 {
            Speed::Low
        } else if self.status & 1 << 10 != 0 {
            Speed::High
        } else {
            Speed::Full
        }
    }

    /// The connection changed: a device came or went.
    pub fn connect_changed(&self) -> bool {
        self.change & 1 != 0
    }

    /// Over-current changed.
    pub fn over_current_changed(&self) -> bool {
        self.change & 1 << 3 != 0
    }

    /// A reset finished: the reset's, or on a USB 3 hub a warm reset's.
    pub fn reset_changed(&self) -> bool {
        self.change & if self.usb3 { 1 << 4 | 1 << 5 } else { 1 << 4 } != 0
    }

    /// The C_ features to clear to acknowledge every change set.
    pub fn changes(&self) -> impl Iterator<Item = u16> + use<> {
        use feature::*;
        const USB2: [(u16, u16); 5] = [
            (1, C_PORT_CONNECTION),
            (1 << 1, C_PORT_ENABLE),
            (1 << 2, C_PORT_SUSPEND),
            (1 << 3, C_PORT_OVER_CURRENT),
            (1 << 4, C_PORT_RESET),
        ];
        const USB3: [(u16, u16); 6] = [
            (1, C_PORT_CONNECTION),
            (1 << 3, C_PORT_OVER_CURRENT),
            (1 << 4, C_PORT_RESET),
            (1 << 5, C_BH_PORT_RESET),
            (1 << 6, C_PORT_LINK_STATE),
            (1 << 7, C_PORT_CONFIG_ERROR),
        ];
        let change = self.change;
        let table: &'static [(u16, u16)] = if self.usb3 { &USB3 } else { &USB2 };
        table.iter().filter(move |&&(bit, _)| change & bit != 0).map(|&(_, feature)| feature)
    }
}

impl Setup {
    /// GET_DESCRIPTOR for a hub's own descriptor, a USB 3 hub's with
    /// `usb3`: the first `length` bytes.
    pub fn hub_descriptor(usb3: bool, length: u16) -> Setup {
        let kind = if usb3 { descriptor::SUPERSPEED_HUB } else { descriptor::HUB };
        Setup {
            request_type: DEVICE_TO_HOST | CLASS | TO_DEVICE,
            request: request::GET_DESCRIPTOR,
            value: u16::from(kind) << 8,
            index: 0,
            length,
        }
    }

    /// GET_STATUS for the hub itself: wHubStatus (bit 1 over-current) and
    /// wHubChange (bit 0 local power, bit 1 over-current).
    pub fn hub_status() -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | CLASS | TO_DEVICE,
            request: request::GET_STATUS,
            value: 0,
            index: 0,
            length: 4,
        }
    }

    /// CLEAR_FEATURE `feature` on the hub itself.
    pub fn clear_hub_feature(feature: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_DEVICE,
            request: request::CLEAR_FEATURE,
            value: feature,
            index: 0,
            length: 0,
        }
    }

    /// GET_STATUS for hub port `port`: its [`PortStatus`].
    pub fn port_status(port: u8) -> Setup {
        Setup {
            request_type: DEVICE_TO_HOST | CLASS | TO_OTHER,
            request: request::GET_STATUS,
            value: 0,
            index: u16::from(port),
            length: PortStatus::SIZE,
        }
    }

    /// SET_FEATURE `feature` on hub port `port`.
    pub fn set_port_feature(port: u8, feature: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_OTHER,
            request: request::SET_FEATURE,
            value: feature,
            index: u16::from(port),
            length: 0,
        }
    }

    /// CLEAR_FEATURE `feature` on hub port `port`.
    pub fn clear_port_feature(port: u8, feature: u16) -> Setup {
        Setup {
            request_type: CLASS | TO_OTHER,
            request: request::CLEAR_FEATURE,
            value: feature,
            index: u16::from(port),
            length: 0,
        }
    }

    /// SET_HUB_DEPTH: tells a USB 3 hub how many hubs are above it (0 on a
    /// root port).
    pub fn set_hub_depth(depth: u8) -> Setup {
        Setup {
            request_type: CLASS | TO_DEVICE,
            request: SET_HUB_DEPTH,
            value: u16::from(depth),
            index: 0,
            length: 0,
        }
    }
}

/// How many hubs a route string goes through: its ports, a nibble each
/// from the lowest, up to the first that is 0.
pub fn depth(route: u32) -> usize {
    (0..MAX_DEPTH).take_while(|&i| (route >> (4 * i)) & 0xf != 0).count()
}

/// The route string of the device on port `port` of the hub at `route`,
/// if the tree is not already as deep as it can be.
pub fn child_route(route: u32, port: u8) -> Option<u32> {
    let levels = depth(route);
    (levels < MAX_DEPTH && (1..=MAX_PORTS).contains(&port))
        .then(|| route | u32::from(port) << (4 * levels))
}

/// Whether the device at `route` is below the hub at `hub`, on the same
/// root port: `hub`'s route starts `route`, which goes further.
pub fn is_below(route: u32, hub: u32) -> bool {
    let levels = depth(hub);
    depth(route) > levels && route & ((1 << (4 * levels)) - 1) == hub
}
