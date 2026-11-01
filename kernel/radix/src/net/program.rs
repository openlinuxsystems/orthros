// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Network cards a program drives (ADR 0198): the kernel's side of the
//! link to the driver (`radix_nic::link`).
//!
//! With `e1000=program`, a card of radix-intel-net's `em` family that an
//! IOMMU isolates is left to a driver program (`user::device`), and gets
//! its interface at boot as any card does, with a [`Program`] under its
//! stack. Until a driver attaches it the link is down: nothing is
//! received, and nothing can be sent. `attach_network` maps a [`Pages`]
//! link into the driver's space and hands it to the interface
//! ([`attach`]); from then on the stack takes the frames the driver puts
//! there and puts its own for the driver, whom it wakes when it does.
//! When the driver's device capability goes, [`detach`] takes the link
//! back first, and the interface stays as it is, down, for the next one.

use alloc::sync::Arc;

use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::Kmalloc;
use radix_nic::link::{self, Memory};

use super::{Interface, numbered};
use crate::Arch;

/// A link's shared pages: [`link::SIZE`] bytes of kernel memory the
/// driver maps, freed once neither the driver's space nor the interface
/// holds them.
pub struct Pages(<Arch as UserMode>::Shared);

/// The pages of a link.
pub const PAGES: usize = link::SIZE / radix_nic::PAGE;

impl Pages {
    /// A new link, zeroed, with its header written; `None` without the
    /// memory.
    pub fn new() -> Option<Pages> {
        let pages = Pages(Arch::new_shared(PAGES)?);
        link::init(&pages);
        Some(pages)
    }

    /// The physical address of its page `page`, to map it.
    pub fn phys(&self, page: usize) -> u64 {
        Arch::shared_phys(&self.0, page)
    }
}

impl Memory for Pages {
    fn load(&self, offset: usize) -> u32 {
        Arch::shared_load(&self.0, offset)
    }

    fn store(&self, offset: usize, value: u32) {
        Arch::shared_store(&self.0, offset, value);
    }

    fn read(&self, offset: usize, out: &mut [u8]) {
        Arch::shared_read(&self.0, offset, out);
    }

    fn write(&self, offset: usize, data: &[u8]) {
        Arch::shared_write(&self.0, offset, data);
    }
}

/// A link a driver attached.
struct Attached {
    pages: Arc<Pages, Kmalloc>,
    side: link::Stack,
}

/// The card under the stack of an interface whose card a program drives.
pub struct Program {
    /// Its place among the cards left for programs (`user::device`).
    left: usize,
    /// Its MAC address: a random one until a driver gives the card's.
    mac: [u8; 6],
    attached: Option<Attached>,
}

impl Program {
    /// The card left for programs at `left`, with `mac` until a driver
    /// attaches it.
    pub fn new(left: usize, mac: [u8; 6]) -> Program {
        Program { left, mac, attached: None }
    }

    /// Its MAC address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Whether its link is up: a driver attached it, keeps to the rules,
    /// and says the card's link is.
    pub fn up(&self) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|at| !at.side.broken() && at.side.link(&*at.pages).up)
    }

    /// Copies the next frame the driver received into `frame`.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        let at = self.attached.as_mut()?;
        at.side.receive(&*at.pages, frame)
    }

    /// Whether a frame to send has a slot.
    pub fn can_send(&mut self) -> bool {
        self.attached.as_mut().is_some_and(|at| at.side.can_send(&*at.pages))
    }

    /// Puts `frame` in a slot for the driver.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        self.attached.as_mut().is_some_and(|at| at.side.send(&*at.pages, frame))
    }

    /// Shows the driver the frames put in since the last time, and wakes
    /// it if there were any.
    pub fn flush(&mut self) {
        if let Some(at) = self.attached.as_mut()
            && at.side.flush(&*at.pages)
        {
            crate::user::device::signal(self.left, radix_abi::device::EVENT_SEND);
        }
    }
}

/// The interface whose card is the card left at `left`, numbered
/// `number` among the cards.
fn interface(number: usize, left: usize) -> Option<&'static Interface> {
    let interface = numbered(number + 1)?;
    let ours = matches!(&*interface.stack.lock().link(), super::Card::Program(p) if p.left == left);
    ours.then_some(interface)
}

/// Attaches the link `pages` to the interface of the card left at `left`,
/// card number `number`, whose MAC address is `mac`, for the driver
/// `driver`: false if it has one already.
pub fn attach(
    number: usize,
    left: usize,
    pages: Arc<Pages, Kmalloc>,
    mac: [u8; 6],
    driver: &str,
) -> bool {
    let Some(interface) = interface(number, left) else { return false };
    {
        let mut stack = interface.stack.lock();
        let super::Card::Program(card) = stack.link() else { return false };
        if card.attached.is_some() {
            return false;
        }
        card.attached = Some(Attached { pages, side: link::Stack::new() });
        card.mac = mac;
        stack.set_mac(mac);
    }
    let [a, b, c, d, e, f] = mac;
    info!(
        "Net: {} is attached to its driver, {driver}, MAC {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}",
        interface.name
    );
    interface.wake();
    true
}

/// Takes the link back from the interface of the card left at `left`,
/// card number `number`: nothing more goes through it. Says whether the
/// driver broke the link's rules, and how many of the frames it gave were
/// dropped for a length no frame has, if any.
pub fn detach(number: usize, left: usize) {
    let Some(interface) = interface(number, left) else { return };
    let gone = {
        let mut stack = interface.stack.lock();
        let super::Card::Program(card) = stack.link() else { return };
        card.attached.take()
    };
    let Some(gone) = gone else { return };
    let broke = if gone.side.broken() { ", having broken the link's rules" } else { "" };
    match gone.side.dropped() {
        0 => info!("Net: {} is down: its driver let go of it{broke}", interface.name),
        dropped => info!(
            "Net: {} is down: its driver let go of it{broke}; {dropped} of the frames it gave \
             had a length no frame has, and were dropped",
            interface.name
        ),
    }
    interface.wake();
}

/// Wakes the interface of the card left at `left`, card number `number`:
/// its driver put frames on the link, or changed the card's link.
pub fn wake(number: usize) {
    if let Some(interface) = numbered(number + 1) {
        interface.wake();
    }
}
