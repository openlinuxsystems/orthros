// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! e1000d(8): drives Intel's e1000 and e1000e network cards as a program
//! (ADR 0198), with the kernel's own driver for them (radix-intel-net's
//! `em`) running here, over the device capabilities Spark gives it.
//!
//! With the boot flag `e1000=program`, the kernel leaves each such card
//! that an IOMMU confines to its own memory for a driver program
//! (radix-device(2)), and Spark gives every card its service file names
//! (`devices.pci`) to e1000d, from SLOT_DEVICE on. For each one e1000d
//! maps the card's registers and the memory it reaches, gets its
//! interrupt (an MSI on the PCI Express e1000e; the PCI 8254x are polled,
//! as the kernel polls them), starts it, and attaches it to its network
//! interface. Then a thread per card moves frames between the card and
//! the link the kernel shares with it: what the card received, to the
//! stack; what the stack sends, to the card; and the card's link.
//!
//! If e1000d ends, by a crash or a signal, the kernel stops its cards
//! before it frees their memory, puts their interfaces down, and Spark
//! starts e1000d again, which brings them back up. A card that fails
//! ends e1000d with an error, so that it is started again.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use radix_abi::capability::{self, ObjectKind};
use radix_abi::device::{self, DeviceInfo, INTERRUPT_MSI};
use radix_abi::startup::{DEVICE_SLOTS, SLOT_DEVICE, SLOT_SPACE};
use radix_hw::{Dma, Mmio};
use radix_intel_net::{Card, Interrupt, Kind, Model};
use radix_nic::link::{self, Driver, Memory};
use radix_nic::{Link, MAX_FRAME, MIN_FRAME, Nic, PAGE};

use crate::args::Args;
use crate::sys::{SYS, status_name};
use crate::util::{self, Fail};

/// How long a card thread waits for its card's interrupt, or for frames
/// to send, before it looks at the card's link again.
const LINK_POLL: Duration = Duration::from_millis(500);
/// How long it waits while frames move through a card that is polled, or
/// while frames wait for room: the kernel's own BUSY_NS.
const BUSY: Duration = Duration::from_millis(1);
/// And while a polled card is quiet: the kernel's own IDLE_NS.
const IDLE: Duration = Duration::from_millis(10);

/// A card's PCI address as lspci(8) writes it.
fn pci_address(address: u32) -> String {
    format!(
        "{:04x}:{:02x}:{:02x}.{}",
        address >> 16,
        (address >> 8) & 0xff,
        (address >> 3) & 0x1f,
        address & 7
    )
}

/// What `info` says of the card in `slot`: its PCI address, vendor and
/// device.
fn identity(slot: u32) -> Result<(u32, u16, u16), String> {
    let mut info = [0u8; DeviceInfo::SIZE];
    device::info(&SYS, slot, &mut info).map_err(|s| format!("info: {}", status_name(s.0)))?;
    let u16_at = |at: usize| u16::from_le_bytes([info[at], info[at + 1]]);
    let at = DeviceInfo::OFFSET_ADDRESS;
    let address = u32::from_le_bytes([info[at], info[at + 1], info[at + 2], info[at + 3]]);
    Ok((address, u16_at(DeviceInfo::OFFSET_VENDOR), u16_at(DeviceInfo::OFFSET_DEVICE)))
}

/// A link's memory as e1000d maps it: [`link::SIZE`] bytes from `base`,
/// which the kernel shares with the interface's stack.
struct Mapped {
    base: usize,
}

impl Mapped {
    /// The header's word at `offset`.
    fn word(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= link::HEADER);
        // SAFETY: `attach_network` mapped the link readable and writable
        // at `base` for as long as e1000d holds the card, which it does
        // until it ends; the word is in its header page, 4-aligned, and
        // the kernel reaches the header's words only atomically too.
        unsafe { AtomicU32::from_ptr((self.base + offset) as *mut u32) }
    }

    /// Checks that `len` bytes from `offset` are in the link.
    fn check(offset: usize, len: usize) {
        assert!(offset.checked_add(len).is_some_and(|end| end <= link::SIZE));
    }
}

impl Memory for Mapped {
    fn load(&self, offset: usize) -> u32 {
        self.word(offset).load(Ordering::Acquire)
    }

    fn store(&self, offset: usize, value: u32) {
        self.word(offset).store(value, Ordering::Release);
    }

    fn read(&self, offset: usize, out: &mut [u8]) {
        Self::check(offset, out.len());
        // SAFETY: the bytes are in the mapping (checked); by the link's
        // rules the kernel does not write a slot while this side owns it,
        // and the acquire load of its index ordered its writes before.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self.base + offset) as *const u8,
                out.as_mut_ptr(),
                out.len(),
            );
        }
    }

    fn write(&self, offset: usize, data: &[u8]) {
        Self::check(offset, data.len());
        // SAFETY: as in `read`; the kernel reads the slot only after the
        // release store of this side's index that follows.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                (self.base + offset) as *mut u8,
                data.len(),
            );
        }
    }
}

/// A card set up and attached, for its thread to drive.
struct Running {
    slot: u32,
    name: String,
    card: Card,
    interrupt: Interrupt,
    link: Mapped,
}

/// DMA memory goes back with the card, when its capability goes: nothing
/// to do when the driver drops it.
fn keep(_virt: usize, _phys: u64, _len: usize, _tag: u32) {}

/// A locally administered MAC address from the kernel's random source,
/// for a card that has none of its own.
fn fallback_mac() -> [u8; 6] {
    let mut bytes = [0u8; 6];
    // SAFETY: the buffer is valid for its length.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if got != 6 {
        bytes = util::now_ns().to_le_bytes()[..6].try_into().unwrap_or([2, 0, 0, 0, 0, 1]);
    }
    radix_nic::local_mac(bytes)
}

/// The card in `slot`, `model`, started and attached to its interface.
fn start(slot: u32, address: u32, model: Model) -> Result<Running, String> {
    let at = pci_address(address);
    fn fails(what: &str) -> impl Fn(radix_abi::Status) -> String + '_ {
        move |s| format!("{what}: {}", status_name(s.0))
    }
    let registers = device::map_registers(&SYS, slot, SLOT_SPACE, 0, 0)
        .map_err(fails("mapping its registers"))?;
    let len = usize::try_from(registers.size).map_err(|_| "its registers are too large")?;
    // SAFETY: the kernel mapped the card's first BAR, `len` bytes of
    // device memory, uncached at `registers.at`, for as long as e1000d
    // holds the card, which it does until it ends; nothing else here
    // touches those pages.
    let regs = unsafe { Mmio::new(registers.at as usize, len) };
    // The PCI 8254x are polled, as the kernel polls them.
    let interrupt = if model.msi() {
        let kind = device::interrupt(&SYS, slot).map_err(fails("its interrupt"))?.kind;
        if kind == INTERRUPT_MSI { Interrupt::Msi } else { Interrupt::Polled }
    } else {
        Interrupt::Polled
    };
    let mut alloc = |pages: usize| {
        let given = device::map_dma(&SYS, slot, SLOT_SPACE, 0, u32::try_from(pages).ok()?).ok()?;
        // SAFETY: the kernel mapped `pages` pages of zeroed memory at
        // `given.at`, which follow one another at `given.device_address`
        // where the card reaches them, for this card alone, until its
        // capability goes; `keep` gives back nothing before then.
        Some(unsafe { Dma::new(given.at as usize, given.device_address, pages * PAGE, 0, keep) })
    };
    let mut delay = |us: u32| thread::sleep(Duration::from_micros(us.into()));
    let card =
        radix_intel_net::start(model, regs, interrupt, fallback_mac(), &mut alloc, &mut delay)
            .map_err(|err| format!("{err}"))?;
    let mac = card.info().mac;
    let attached = device::attach_network(&SYS, slot, SLOT_SPACE, 0, &mac)
        .map_err(fails("attaching it to its interface"))?;
    let link = Mapped { base: attached.at as usize };
    if !link::valid(&link) {
        return Err("its link is of a layout e1000d does not know".to_string());
    }
    let name = format!("net{}", attached.interface);
    let how = match interrupt {
        Interrupt::Msi => "an MSI",
        Interrupt::Polled => "polled",
    };
    let [a, b, c, d, e, f] = mac;
    eprintln!(
        "e1000d: {name} is the {} at {at}, MAC {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}, {how}",
        model.name
    );
    Ok(Running { slot, name, card, interrupt, link })
}

/// Drives `running` until its card fails or the kernel breaks its link;
/// returns why it stopped.
fn drive(running: Running) -> String {
    let Running { slot, name, mut card, interrupt, link } = running;
    let link = &link;
    let mut driver = Driver::new();
    let mut frame = [0u8; link::SLOT];
    let mut shown: Option<Link> = None;
    loop {
        let mut wake = false;
        let now = card.link();
        if shown != Some(now) {
            driver.set_link(link, now);
            shown = Some(now);
            wake = true;
        }
        // What the card received, while the link has room: what stays
        // waits in the card, which drops what it cannot hold, as a wire.
        let mut received = 0u32;
        while driver.can_receive(link) {
            let Some(len) = card.receive(&mut frame) else { break };
            driver.received(link, &frame[..len]);
            received += 1;
        }
        let full = !driver.can_receive(link);
        // What the stack sends, while the card has room.
        let mut sent = 0u32;
        while card.can_send() {
            let Some(len) = driver.take(link, &mut frame) else { break };
            // The kernel sends only frames; anything else is dropped.
            if (MIN_FRAME..=MAX_FRAME).contains(&len) && card.send(&frame[..len]) {
                sent += 1;
            }
        }
        if sent > 0 {
            card.flush();
        }
        let waiting = driver.sending(link);
        if received > 0 || wake {
            let _ = device::wake(&SYS, slot);
        }
        if driver.broken() {
            return format!("{name}: the kernel broke the link's rules");
        }
        let moved = received + sent > 0;
        // A card that moved nothing is asked whether it failed only then:
        // asking reads a register.
        if !moved && card.failed() {
            return format!("{name}: the card stopped with an error");
        }
        let timeout = if full || waiting {
            BUSY
        } else {
            match interrupt {
                Interrupt::Msi => LINK_POLL,
                Interrupt::Polled if moved => BUSY,
                Interrupt::Polled => IDLE,
            }
        };
        let ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        // An interrupted wait (a signal) looks again at once.
        let _ = device::wait(&SYS, slot, ns);
    }
}

/// e1000d(8).
pub fn main(args: &mut Args) -> Result<i32, Fail> {
    if let Some(arg) = args.next()? {
        return Err(args.unknown(&arg));
    }
    let slots = (SLOT_DEVICE..SLOT_DEVICE + DEVICE_SLOTS)
        .filter(|&slot| capability::info(&SYS, slot).is_ok_and(|i| i.kind == ObjectKind::Device));
    let mut cards = Vec::new();
    for slot in slots {
        let (address, vendor, device) = identity(slot).map_err(util::fail)?;
        let at = pci_address(address);
        let model = radix_intel_net::identify(vendor, device)
            .filter(|model| matches!(model.kind, Kind::Em(_)))
            .ok_or_else(|| {
                util::fail(format!("{at} ({vendor:04x}:{device:04x}) is no e1000 or e1000e card"))
            })?;
        let running =
            start(slot, address, model).map_err(|why| util::fail(format!("{at}: {why}")))?;
        cards.push(running);
    }
    if cards.is_empty() {
        return Err(util::fail("Spark gave it no card to drive".to_string()));
    }
    // Each card on a thread of its own; the first to stop ends e1000d,
    // whose cards the kernel then stops, and Spark starts it again.
    let (tell, stopped) = mpsc::channel();
    for running in cards {
        let tell = tell.clone();
        let name = running.name.clone();
        thread::Builder::new()
            .name(format!("e1000d {name}"))
            .spawn(move || {
                let _ = tell.send(drive(running));
            })
            .map_err(|err| util::failed(&"a card's thread", err))?;
    }
    drop(tell);
    let why = stopped.recv().unwrap_or_else(|_| "every card's thread ended".to_string());
    Err(util::fail(why))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_addresses_are_written_as_lspci_writes_them() {
        assert_eq!(pci_address(0x0000_0018), "0000:00:03.0");
        assert_eq!(pci_address(0x0001_0a12), "0001:0a:02.2");
    }

    #[test]
    fn the_link_is_reached_within_its_mapping() {
        let mut memory = vec![0u64; link::SIZE / 8];
        let link = Mapped { base: memory.as_mut_ptr() as usize };
        link::init(&link);
        assert!(link::valid(&link));
        let (mut driver, mut stack) = (Driver::new(), link::Stack::new());
        assert!(driver.received(&link, &[7u8; 60]));
        let mut frame = [0u8; link::SLOT];
        assert_eq!(stack.receive(&link, &mut frame), Some(60));
        assert!(stack.send(&link, &[9u8; 1514]));
        assert!(stack.flush(&link));
        assert_eq!(driver.take(&link, &mut frame), Some(1514));
        assert!(frame[..1514].iter().all(|&b| b == 9));
    }
}
