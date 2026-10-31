// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The switch that moves the USB 2.0 ports of Intel's 7, 8 and 9 Series
//! chipsets and of Bay Trail from their EHCI controllers to their xHCI
//! controller (ADR 0243).
//!
//! On these chipsets each USB 2.0 port is wired to an EHCI controller
//! and to the xHCI one, and four registers in the xHCI function's
//! configuration space choose (Intel's *8 Series/C220 Series Chipset
//! Family Platform Controller Hub Datasheet*, 17.1.33 to 17.1.36):
//! XUSB2PR (0xD0) routes each USB 2.0 port to the xHCI controller whose
//! bit is set, XUSB2PRM (0xD4) says which ports the system may route,
//! USB3_PSSEN (0xD8) turns on each USB 3 port's SuperSpeed terminations,
//! and USB3PRM (0xDC) says which of those the system may turn on. The
//! firmware leaves the ports on the EHCI controllers unless it is set to
//! hand them over itself ("xHCI mode: Enabled"); left there, the USB 3
//! ports run at USB 2.0's speed, behind the EHCI controllers' own hubs.
//!
//! The switch is made, as Linux makes it, on an Intel xHCI controller
//! when an Intel EHCI controller sits on the same machine: every port the
//! masks allow goes to the xHCI controller. Newer chipsets have no EHCI
//! controller, and the registers are something else there. Before the
//! machine powers off or restarts, the ports are routed back as the
//! firmware left them ([`restore`]): some 7 and 8 Series machines restart
//! or wake at once if they are powered off with their ports moved.

use radix_hw::pci::Function;

/// Intel's PCI vendor ID.
pub const INTEL: u16 = 0x8086;

/// The registers in the xHCI function's configuration space.
const XUSB2PR: usize = 0xd0;
const XUSB2PRM: usize = 0xd4;
const USB3_PSSEN: usize = 0xd8;
const USB3PRM: usize = 0xdc;

/// How the ports are routed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Routing {
    /// The USB 2.0 ports routed to the xHCI controller, a bit each.
    pub usb2: u32,
    /// The USB 3 ports whose SuperSpeed terminations are on.
    pub usb3: u32,
}

/// How the ports of the Intel xHCI controller `function` are routed.
pub fn routing(function: &Function) -> Routing {
    Routing { usb2: function.read32(XUSB2PR), usb3: function.read32(USB3_PSSEN) }
}

/// Whether `function` is an Intel xHCI controller whose ports may be
/// switched, given whether an Intel EHCI controller is on the machine.
pub fn switchable(function: &Function, intel_ehci: bool) -> bool {
    intel_ehci && function.vendor_id() == INTEL && function.class() == (0x0c, 0x03, 0x30)
}

/// Whether `function` is an Intel EHCI controller.
pub fn is_intel_ehci(function: &Function) -> bool {
    function.vendor_id() == INTEL && function.class() == crate::ehci::PCI_CLASS
}

/// Routes every port the masks of the Intel xHCI controller `function`
/// allow to it, and turns on their SuperSpeed terminations; returns how
/// they are routed after.
pub fn switch_ports(function: &Function) -> Routing {
    let usb3 = function.read32(USB3PRM);
    function.write32(USB3_PSSEN, usb3);
    let usb2 = function.read32(XUSB2PRM);
    function.write32(XUSB2PR, usb2);
    routing(function)
}

/// Routes the ports of `function` as `before` says: as the firmware left
/// them, before [`switch_ports`].
pub fn restore(function: &Function, before: Routing) {
    function.write32(XUSB2PR, before.usb2);
    function.write32(USB3_PSSEN, before.usb3);
}
