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
//!
//! A USB Ethernet adapter (ADR 0199) has the same link, with the thread
//! of its USB controller on the driver's side: [`attach_usb`] gives it an
//! interface when it is set up, with the link attached at once, and
//! [`detach_usb`] takes the link back when it is unplugged. Plugged in
//! again, it gets its interface back.

use alloc::sync::Arc;

use core::sync::atomic::{AtomicBool, Ordering};

use radix_hal::UserMode;
use radix_klog::info;
use radix_mm::{Flags, HEAP, Kmalloc};
use radix_nic::link::{self, Memory};
use radix_sync::Mutex;

use super::{COUNT, Card, INTERFACES, Interface, MAX_CARDS, add_interface, numbered, run_thread};
use crate::Arch;

/// A link's shared pages: [`link::SIZE`] bytes of kernel memory the
/// driver maps, freed once neither the driver's space nor the interface
/// holds them.
pub struct Pages {
    shared: <Arch as UserMode>::Shared,
    /// Set by the thread of a USB controller that found the link full of
    /// what its adapter received: the stack wakes it once it takes a
    /// frame.
    wants_room: AtomicBool,
}

/// The pages of a link.
pub const PAGES: usize = link::SIZE / radix_nic::PAGE;

impl Pages {
    /// A new link, zeroed, with its header written; `None` without the
    /// memory.
    pub fn new() -> Option<Pages> {
        let pages = Pages { shared: Arch::new_shared(PAGES)?, wants_room: AtomicBool::new(false) };
        link::init(&pages);
        Some(pages)
    }

    /// The physical address of its page `page`, to map it.
    pub fn phys(&self, page: usize) -> u64 {
        Arch::shared_phys(&self.shared, page)
    }

    /// Asks to be woken when the stack takes a received frame: the link
    /// is full. A USB controller's thread does.
    pub fn want_room(&self) {
        self.wants_room.store(true, Ordering::SeqCst);
    }
}

impl Memory for Pages {
    fn load(&self, offset: usize) -> u32 {
        Arch::shared_load(&self.shared, offset)
    }

    fn store(&self, offset: usize, value: u32) {
        Arch::shared_store(&self.shared, offset, value);
    }

    fn read(&self, offset: usize, out: &mut [u8]) {
        Arch::shared_read(&self.shared, offset, out);
    }

    fn write(&self, offset: usize, data: &[u8]) {
        Arch::shared_write(&self.shared, offset, data);
    }
}

/// A link a driver attached.
struct Attached {
    pages: Arc<Pages, Kmalloc>,
    side: link::Stack,
}

/// Who drives a card through its link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Driver {
    /// A program, through the card left for programs at this place
    /// (`user::device`).
    Program(usize),
    /// The thread of the USB controller with this index, for an adapter
    /// on one of its ports (`usb::net`).
    Usb(usize),
}

/// The card under the stack of an interface whose card a program drives,
/// or the thread of a USB controller.
pub struct Program {
    /// Who drives it.
    driver: Driver,
    /// Its MAC address: a random one until a driver gives the card's.
    mac: [u8; 6],
    attached: Option<Attached>,
}

impl Program {
    /// The card left for programs at `left`, with `mac` until a driver
    /// attaches it.
    pub fn new(left: usize, mac: [u8; 6]) -> Program {
        Program { driver: Driver::Program(left), mac, attached: None }
    }

    /// Its MAC address.
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Whether its link is up: a driver attached it, keeps to the rules,
    /// and says the card's link is.
    pub fn up(&self) -> bool {
        self.attached.as_ref().is_some_and(|at| !at.side.broken() && at.side.link(&*at.pages).up)
    }

    /// Copies the next frame the driver received into `frame`; wakes a
    /// USB controller's thread that waits for the room.
    pub fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        let at = self.attached.as_mut()?;
        let len = at.side.receive(&*at.pages, frame)?;
        if let Driver::Usb(controller) = self.driver
            && at.pages.wants_room.swap(false, Ordering::SeqCst)
        {
            crate::usb::wake_controller(controller);
        }
        Some(len)
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
            match self.driver {
                Driver::Program(left) => {
                    crate::user::device::signal(left, radix_abi::device::EVENT_SEND);
                }
                Driver::Usb(controller) => crate::usb::wake_controller(controller),
            }
        }
    }
}

/// The interface whose card is the card left at `left`, numbered
/// `number` among the cards.
fn interface(number: usize, left: usize) -> Option<&'static Interface> {
    let interface = numbered(number + 1)?;
    let ours = matches!(
        &*interface.stack.lock().link(),
        Card::Program(p) if p.driver == Driver::Program(left)
    );
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

/// Held while a USB adapter is given an interface: the threads of two
/// controllers could otherwise take the same number.
static USB_ADDING: Mutex<()> = Mutex::new(());

/// Gives the USB Ethernet adapter `what` (its protocol's name), of MAC
/// address `mac`, on `place` of the USB controller with index
/// `controller`, an interface whose link is attached at once: the one it
/// had if it was plugged in before, else a new one. Returns the card's
/// number among the cards, and the link's pages for the controller's
/// thread; `None` without a free interface or the memory. Its frame,
/// and `add_interface`'s, are too big for a thread's usual stack: an xHCI
/// controller's thread has a bigger one.
#[inline(never)]
pub fn attach_usb(
    controller: usize,
    mac: [u8; 6],
    what: &str,
    place: &dyn core::fmt::Display,
) -> Option<(usize, Arc<Pages, Kmalloc>)> {
    let _adding = USB_ADDING.lock();
    let pages = Arc::try_new_in(Pages::new()?, HEAP.with(Flags::MAY_SLEEP)).ok()?;
    let [a, b, c, d, e, f] = mac;
    let count = COUNT.load(Ordering::Acquire);
    // Its interface from before: a USB adapter's with its address, which
    // no adapter has now.
    let before = (0..count).find(|&index| {
        INTERFACES[index].get().is_some_and(|interface| {
            matches!(
                &*interface.stack.lock().link(),
                Card::Program(p) if matches!(p.driver, Driver::Usb(_))
                    && p.attached.is_none()
                    && p.mac == mac
            )
        })
    });
    if let Some(index) = before {
        let interface = INTERFACES[index].get()?;
        {
            let mut stack = interface.stack.lock();
            let Card::Program(card) = stack.link() else { return None };
            card.driver = Driver::Usb(controller);
            card.attached = Some(Attached { pages: pages.clone(), side: link::Stack::new() });
        }
        info!(
            "Net: {} is back: the USB {what} network adapter on {place}, MAC \
             {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}",
            interface.name
        );
        interface.wake();
        return Some((index, pages));
    }
    if count >= MAX_CARDS {
        return None;
    }
    let card = Program {
        driver: Driver::Usb(controller),
        mac,
        attached: Some(Attached { pages: pages.clone(), side: link::Stack::new() }),
    };
    let interface = add_interface(count, Card::Program(card)).ok()?;
    info!(
        "Net: {} is a USB {what} network adapter on {place}, MAC \
         {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}",
        interface.name
    );
    run_thread(interface).ok()?;
    Some((count, pages))
}

/// Takes the link back from the interface of the USB adapter that is card
/// number `number`, unplugged: the interface goes down, and keeps its
/// addresses for when the adapter comes back.
pub fn detach_usb(number: usize) {
    let Some(interface) = numbered(number + 1) else { return };
    let gone = {
        let mut stack = interface.stack.lock();
        let Card::Program(card) = stack.link() else { return };
        card.attached.take()
    };
    if gone.is_some() {
        info!("Net: {} is down: its USB adapter is gone", interface.name);
        interface.wake();
    }
}
