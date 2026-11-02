// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Broadcom's NetXtreme gigabit Ethernet controllers (ADR 0197): the
//! BCM5717 family of servers' LAN-on-motherboard and cards (the BCM5717,
//! BCM5718, BCM5719 and BCM5720), and the desktop and laptop chips of
//! Broadcom's 57785 programmer's guide: the BCM5761 and BCM5762, the
//! BCM57765 family (BCM57761, BCM57762, BCM57765, BCM57766, BCM57781,
//! BCM57782, BCM57785, BCM57786) and the BCM57780. FreeBSD's `bge(4)`
//! drives them all, with many older chips this driver leaves out.
//!
//! One PCI device ID stands for a family whose chip, and its revision,
//! the PCI configuration space says ([`Chip`]); the driver refuses a chip
//! it does not know, a fibre port (SerDes), and a function that is not
//! PCI Express. It resets the chip, waits for its bootcode to say it is
//! done, reads the MAC address the bootcode left in the chip's memory,
//! and sets up its state machines as FreeBSD's `bge_blockinit()` does,
//! with the fixes it applies for each chip. The chips the APE, a
//! processor that may share the port with a management controller, is
//! in take its locks and tell its firmware when the driver starts and
//! stops ([`ape`]).
//!
//! Frames are copied through 256 slots each way ([`rings`]), with no
//! offloads, no VLAN tag stripping, and every multicast frame taken,
//! which IPv6 and mDNS need. The chip writes a status block that says
//! how far it received and sent; with an MSI it raises it once and waits
//! to be told, through its interrupt mailbox, that the driver has seen
//! the status block, by its tag on the BCM5717 family (tagged status),
//! as FreeBSD's does. The MAC is set to the link the PHY negotiated,
//! which the driver asks once a second while the link is down, and
//! when the chip says it changed ([`phy`]).
//!
//! The driver is written from FreeBSD's `bge(4)` and `brgphy(4)`
//! (BSD-4-Clause, Bill Paul and Wind River Systems) as
//! the reference for the registers, the families and their quirks
//! (credits/freebsd-network-drivers.md); no code is taken from them. It
//! loads no firmware: the chips' bootcode is in their NVRAM.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod ape;
mod init;
mod phy;
mod reg;
mod rings;

use radix_hw::pci::Function;
use radix_hw::{Dma, Mmio};
use radix_nic::{Error, Info, Interrupt, Link, Nic, usable_mac};

use crate::ape::{Ape, State};
use crate::phy::Phy;
use crate::rings::{Rings, SLOTS};

/// Broadcom's PCI vendor ID.
pub const VENDOR: u16 = 0x14e4;
/// Dell's, whose boards light the link LED otherwise.
const DELL: u16 = 0x1028;

/// The PCI device IDs this driver knows.
pub mod device {
    /// The BCM5717: two gigabit ports for servers.
    pub const BCM5717: u16 = 0x1655;
    /// The BCM5717's C0 revision, which is a BCM5720 inside.
    pub const BCM5717C: u16 = 0x1665;
    /// The BCM5718: the BCM5717's single port.
    pub const BCM5718: u16 = 0x1656;
    /// The BCM5719: four gigabit ports.
    pub const BCM5719: u16 = 0x1657;
    /// The BCM5720: two gigabit ports, on many Dell PowerEdge and HPE
    /// ProLiant servers.
    pub const BCM5720: u16 = 0x165f;
    /// The BCM5761: desktop boards' gigabit Ethernet.
    pub const BCM5761: u16 = 0x1681;
    /// The BCM5761E, the same.
    pub const BCM5761E: u16 = 0x1680;
    /// The BCM5762: desktop boards' gigabit Ethernet.
    pub const BCM5762: u16 = 0x1687;
    /// The BCM57761, of the BCM57765 family: laptops' and Apple's
    /// gigabit Ethernet, with a card reader beside it in some.
    pub const BCM57761: u16 = 0x16b0;
    /// The BCM57762, of the BCM57766's half of the family.
    pub const BCM57762: u16 = 0x1682;
    /// The BCM57765.
    pub const BCM57765: u16 = 0x16b4;
    /// The BCM57766, on Apple's Macs with a T2 chip.
    pub const BCM57766: u16 = 0x1686;
    /// The BCM57781.
    pub const BCM57781: u16 = 0x16b1;
    /// The BCM57782.
    pub const BCM57782: u16 = 0x16b7;
    /// The BCM57785.
    pub const BCM57785: u16 = 0x16b5;
    /// The BCM57786.
    pub const BCM57786: u16 = 0x16b3;
    /// The BCM57780: laptops' gigabit Ethernet.
    pub const BCM57780: u16 = 0x1692;
}

/// The chips, by FreeBSD's ASIC revisions: the high bits of the chip ID.
mod asic {
    pub const BCM5717: u32 = 0x5717;
    pub const BCM5719: u32 = 0x5719;
    pub const BCM5720: u32 = 0x5720;
    pub const BCM5761: u32 = 0x5761;
    pub const BCM5762: u32 = 0x5762;
    pub const BCM57765: u32 = 0x57785;
    pub const BCM57766: u32 = 0x57766;
    pub const BCM57780: u32 = 0x57780;
    /// What the old register says when the chip's ID is in one of the
    /// product ID registers.
    pub const USE_PRODID_REG: u32 = 0x0f;
}

/// Chip IDs that have quirks of their own.
mod chipid {
    pub const BCM5717_A0: u32 = 0x0571_7000;
    pub const BCM5719_A0: u32 = 0x0571_9000;
    pub const BCM5720_A0: u32 = 0x0572_0000;
    pub const BCM57765_A0: u32 = 0x5778_5000;
    /// The 57765's A revisions, by their chip revision.
    pub const BCM57765_AX: u32 = 0x57_7850;
}

/// A card this driver knows, by its PCI device ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Its name.
    pub name: &'static str,
    /// Its PCI device ID.
    pub device: u16,
    /// Which of the product ID registers says its chip.
    product_id: usize,
}

/// The register that says which chip the 5717 family and the BCM5762
/// are, and the BCM57765 family's.
const GEN2: usize = reg::pci::GEN2_PRODID_ASICREV;
const GEN15: usize = reg::pci::GEN15_PRODID_ASICREV;
const OLD: usize = reg::pci::PRODID_ASICREV;

const CARDS: &[Model] = &[
    Model { name: "Broadcom BCM5717", device: device::BCM5717, product_id: GEN2 },
    Model { name: "Broadcom BCM5717", device: device::BCM5717C, product_id: GEN2 },
    Model { name: "Broadcom BCM5718", device: device::BCM5718, product_id: GEN2 },
    Model { name: "Broadcom BCM5719", device: device::BCM5719, product_id: GEN2 },
    Model { name: "Broadcom BCM5720", device: device::BCM5720, product_id: GEN2 },
    Model { name: "Broadcom BCM5761", device: device::BCM5761, product_id: OLD },
    Model { name: "Broadcom BCM5761E", device: device::BCM5761E, product_id: OLD },
    Model { name: "Broadcom BCM5762", device: device::BCM5762, product_id: GEN2 },
    Model { name: "Broadcom BCM57761", device: device::BCM57761, product_id: GEN15 },
    Model { name: "Broadcom BCM57762", device: device::BCM57762, product_id: GEN15 },
    Model { name: "Broadcom BCM57765", device: device::BCM57765, product_id: GEN15 },
    Model { name: "Broadcom BCM57766", device: device::BCM57766, product_id: GEN15 },
    Model { name: "Broadcom BCM57781", device: device::BCM57781, product_id: GEN15 },
    Model { name: "Broadcom BCM57782", device: device::BCM57782, product_id: GEN15 },
    Model { name: "Broadcom BCM57785", device: device::BCM57785, product_id: GEN15 },
    Model { name: "Broadcom BCM57786", device: device::BCM57786, product_id: GEN15 },
    Model { name: "Broadcom BCM57780", device: device::BCM57780, product_id: OLD },
];

/// The model of the card with PCI IDs `vendor` and `device`, if this
/// driver knows it.
pub fn identify(vendor: u16, device: u16) -> Option<Model> {
    if vendor != VENDOR {
        return None;
    }
    CARDS.iter().copied().find(|model| model.device == device)
}

impl Model {
    /// The article its name takes.
    pub fn article(&self) -> &'static str {
        "a"
    }

    /// Whether it should have an MSI: all of these do.
    pub fn msi(&self) -> bool {
        true
    }

    /// Whether its chip has an APE, whose registers are in the third
    /// BAR, which the kernel then maps too.
    pub fn ape(&self) -> bool {
        matches!(
            self.device,
            device::BCM5717
                | device::BCM5717C
                | device::BCM5718
                | device::BCM5719
                | device::BCM5720
                | device::BCM5761
                | device::BCM5761E
                | device::BCM5762
        )
    }
}

/// The chip, as the driver found it.
struct Chip {
    model: Model,
    /// Its PCI function, whose configuration space the driver reaches.
    function: Function,
    /// Its ID, revision and all, and the ASIC revision that names it.
    chipid: u32,
    asicrev: u32,
    /// The PHY's address on the MDIO bus.
    phy_address: u16,
    /// Where its PCI Express capability is.
    express: usize,
    /// The largest payload the function supports, and the largest read
    /// request the driver lets it make.
    max_payload: u32,
    max_read: u32,
    ape: Option<Ape>,
    /// Whether the status block's tag acknowledges the interrupt.
    tagged: bool,
    /// Whether the board is Dell's.
    dell: bool,
}

impl Chip {
    fn is(&self, asics: &[u32]) -> bool {
        asics.contains(&self.asicrev)
    }

    /// The BCM5717 family's three: FreeBSD's checks for the 5717, 5719
    /// and 5720, whose rings are elsewhere in the chip's memory.
    fn bcm571x(&self) -> bool {
        self.is(&[asic::BCM5717, asic::BCM5719, asic::BCM5720])
    }

    /// FreeBSD's `BGE_IS_5717_PLUS`.
    fn plus_5717(&self) -> bool {
        self.bcm571x() || self.plus_57765()
    }

    /// FreeBSD's `BGE_IS_57765_PLUS`, which counts the BCM5762.
    fn plus_57765(&self) -> bool {
        self.is(&[asic::BCM5762, asic::BCM57765, asic::BCM57766])
    }

    /// Whether it has the jumbo ring, which is kept disabled.
    fn jumbo(&self) -> bool {
        self.plus_5717() && self.chipid != chipid::BCM5719_A0
    }

    /// Whether its read DMA miscounts the room for the first frames it
    /// sends (FreeBSD's `BGE_FLAG_RDMA_BUG`).
    fn rdma_bug(&self) -> bool {
        self.is(&[asic::BCM5719, asic::BCM5720])
    }

    /// Whether it can segment TCP, which FreeBSD sets its DMA engines up
    /// for whether or not it does: the BCM5717 family's way, or the
    /// older chips'.
    fn tso3(&self) -> bool {
        self.plus_5717() && self.chipid != chipid::BCM5719_A0
    }

    fn tso(&self) -> bool {
        !self.plus_5717()
    }

    /// How many descriptors its return ring has.
    fn return_count(&self) -> usize {
        if self.plus_5717() { 1024 } else { 512 }
    }

    /// Whether its PHY gets FreeBSD's fixes after a reset: the BCM5761's
    /// and the BCM57780's do, the newer ones need none.
    fn phy_fixups(&self) -> bool {
        self.is(&[asic::BCM5761, asic::BCM57780])
    }

    /// Whether its PHY's DSP needs the jitter fix.
    fn jitter_bug(&self) -> bool {
        self.asicrev == asic::BCM5761
    }

    /// Whether the APE's firmware runs.
    fn on_ape(&self) -> bool {
        self.ape.as_ref().is_some_and(Ape::running)
    }

    /// Reads the word at `at` in the chip's memory, through the
    /// configuration space's window.
    fn read_memory(&self, at: usize) -> u32 {
        self.function.write32(reg::pci::MEMWIN_BASEADDR, at as u32);
        let value = self.function.read32(reg::pci::MEMWIN_DATA);
        self.function.write32(reg::pci::MEMWIN_BASEADDR, 0);
        value
    }

    /// Writes the word at `at` in the chip's memory, likewise.
    fn write_memory(&self, at: usize, value: u32) {
        self.function.write32(reg::pci::MEMWIN_BASEADDR, at as u32);
        self.function.write32(reg::pci::MEMWIN_DATA, value);
        self.function.write32(reg::pci::MEMWIN_BASEADDR, 0);
    }
}

/// The chip of `model`, by the IDs in its configuration space.
fn chip_id(model: &Model, function: &Function) -> u32 {
    let id = function.read32(reg::pci::MISC_CTL) >> reg::pci::MISC_CTL_ASICREV_SHIFT;
    if id >> 12 != asic::USE_PRODID_REG {
        return id;
    }
    if model.device == device::BCM5717C {
        // The 5717's C0 is a 5720.
        return chipid::BCM5720_A0;
    }
    function.read32(model.product_id)
}

/// Waits `us` microseconds by `clock`, the time in nanoseconds.
fn spin(clock: fn() -> u64) -> impl FnMut(u32) {
    move |us| {
        let until = clock().saturating_add(u64::from(us) * 1_000);
        while clock() < until {
            core::hint::spin_loop();
        }
    }
}

/// How often the link is looked at, and after how many looks without
/// one the PHY negotiates again: FreeBSD's `MII_ANEGTICKS_GIGE`.
const LINK_EVERY_NS: u64 = 1_000_000_000;
const RENEGOTIATE_AFTER: u32 = 17;
/// How long frames may wait to be sent before the card is taken to have
/// stopped: FreeBSD's `BGE_TX_TIMEOUT`.
const STALL_NS: u64 = 5_000_000_000;

/// Resets the card of `model` and sets it running, with interrupts as
/// `interrupt` says. `function` is its PCI function, `regs` its first
/// BAR, which must reach 64 KiB, and `ape` its third, mapped when
/// [`Model::ape`] says. `fallback_mac` is its address if it has none
/// that can be read; `alloc` gives zeroed DMA memory in pages, below
/// 4 GiB, which no ring then crosses (the chips hang on a DMA across a
/// 4 GiB line); `delay` waits the given number of microseconds; and
/// `clock` is the time in nanoseconds, by which the running card waits
/// and looks at its link.
#[expect(clippy::too_many_arguments, reason = "what the kernel knows of the card")]
pub fn start(
    model: Model,
    function: &Function,
    regs: Mmio,
    ape: Option<Mmio>,
    interrupt: Interrupt,
    fallback_mac: [u8; 6],
    alloc: &mut dyn FnMut(usize) -> Option<Dma>,
    delay: &mut dyn FnMut(u32),
    clock: fn() -> u64,
) -> Result<Card, Error> {
    if regs.len() < reg::END || ape.as_ref().is_some_and(|ape| ape.len() < reg::ape::END) {
        return Err(Error::Registers);
    }
    if regs.read32(reg::MAC_MODE) == u32::MAX {
        return Err(Error::Device("its registers read as all ones"));
    }
    let chipid = chip_id(&model, function);
    let asicrev = chipid >> 12;
    let known = [
        asic::BCM5717,
        asic::BCM5719,
        asic::BCM5720,
        asic::BCM5761,
        asic::BCM5762,
        asic::BCM57765,
        asic::BCM57766,
        asic::BCM57780,
    ];
    if !known.contains(&asicrev) {
        return Err(Error::Unsupported("a chip this driver does not know"));
    }
    let express = function
        .find_capability(radix_hw::pci::capability::EXPRESS)
        .ok_or(Error::Unsupported("PCI Express"))?;
    let pci_function = function.address().function;
    let mut chip = Chip {
        model,
        function: function.clone(),
        chipid,
        asicrev,
        phy_address: 1,
        express,
        max_payload: 128,
        max_read: 4096,
        ape: None,
        tagged: false,
        dell: function.subsystem_vendor_id() == DELL,
    };
    if chip.bcm571x() {
        // The PHY is the function's, after its number: copper at 1 to 4,
        // a SerDes at 8 to 11, which this driver does not drive.
        let serdes = if chipid == chipid::BCM5717_A0 {
            regs.read32(reg::CPMU_PHY_STRAP) & reg::CPMU_PHY_STRAP_IS_SERDES != 0
        } else {
            regs.read32(reg::SGDIG_STS) & reg::SGDIGSTS_IS_SERDES != 0
        };
        if serdes {
            return Err(Error::Unsupported("copper: its port is a SerDes"));
        }
        chip.phy_address = u16::from(pci_function) + 1;
    }
    if let Some(ape) = ape {
        let state = function.read32(reg::pci::PCISTATE);
        function.write32(reg::pci::PCISTATE, state | reg::pci::PCISTATE_ALLOW_APE);
        chip.ape = Some(Ape::new(ape, asicrev == asic::BCM5761, pci_function));
    }
    let capability = function.read16(express + reg::express::DEVICE_CAP);
    chip.max_payload = 128 << (capability & reg::express::CAP_MAX_PAYLOAD);
    chip.max_read = if chip.rdma_bug() { 2048 } else { 4096 };
    init::set_max_read(&chip);
    chip.tagged = interrupt == Interrupt::Msi && chip.plus_5717();

    // As FreeBSD's bge_attach(): reset and set up to read the address.
    init::reset(&regs, &chip, delay);
    if let Some(ape) = &chip.ape {
        ape.tell(State::Unload, delay);
    }
    init::chip_init(&regs, &chip);
    if chip.read_memory(reg::sram::DATA_SIG) == reg::sram::DATA_SIG_MAGIC
        && chip.read_memory(reg::sram::DATA_CFG) & reg::sram::HWCFG_MEDIA == reg::sram::MEDIA_FIBER
    {
        return Err(Error::Unsupported("copper: its port is fibre"));
    }
    let mac = read_mac(&regs, &chip).unwrap_or(fallback_mac);

    // As its bge_init_locked(): reset again, the driver started.
    if let Some(ape) = &chip.ape {
        ape.tell(State::Start, delay);
    }
    init::reset(&regs, &chip, delay);
    init::chip_init(&regs, &chip);
    let rings = Rings::new(regs.clone(), chip.return_count(), alloc)?;
    init::block_init(&regs, &chip, &rings, mac, delay)?;
    init::run(&regs, &chip, &rings, mac, interrupt, delay);
    Phy::new(&regs, &chip, delay).negotiate();
    // Have the chip write its status block now.
    let mode = regs.read32(reg::HCC_MODE);
    regs.write32(reg::HCC_MODE, mode | reg::hcc::COAL_NOW);
    Ok(Card::new(chip, mac, regs, rings, interrupt, clock))
}

/// The MAC address the bootcode left in the chip's memory, or failing
/// that in the MAC's first address registers, if it is one a card can
/// have.
fn read_mac(regs: &Mmio, chip: &Chip) -> Option<[u8; 6]> {
    let high = chip.read_memory(reg::sram::MAC_ADDR_HIGH_MB);
    let (high, low) = if high >> 16 == reg::sram::MAC_ADDR_SIG {
        (high, chip.read_memory(reg::sram::MAC_ADDR_LOW_MB))
    } else {
        (regs.read32(reg::MAC_ADDR1_LO), regs.read32(reg::MAC_ADDR1_HI))
    };
    let [_, _, a, b] = high.to_be_bytes();
    let [c, d, e, f] = low.to_be_bytes();
    let mac = [a, b, c, d, e, f];
    usable_mac(mac).then_some(mac)
}

/// A running card.
pub struct Card {
    regs: Mmio,
    chip: Chip,
    rings: Rings,
    info: Info,
    interrupt: Interrupt,
    clock: fn() -> u64,
    /// The link the MAC is set up for.
    link: Link,
    /// When the link was last looked at, and how many looks in a row
    /// found none.
    looked: Option<u64>,
    down_looks: u32,
    /// Since when frames have waited to be sent with none sent.
    stalled_since: Option<u64>,
    sent: u64,
    /// Whether the 5719's and 5720's read DMA fix is still on.
    rdma_fix: bool,
    stopped: bool,
}

impl Card {
    fn new(
        chip: Chip,
        mac: [u8; 6],
        regs: Mmio,
        rings: Rings,
        interrupt: Interrupt,
        clock: fn() -> u64,
    ) -> Card {
        let info = Info {
            model: chip.model.name,
            mac,
            receive_slots: SLOTS as u16,
            transmit_slots: (SLOTS - 1) as u16,
        };
        let rdma_fix = chip.rdma_bug()
            && regs.read32(reg::RDMA_LSO_CRPTEN_CTRL) & init::tx_length_fix(&chip) != 0;
        Card {
            regs,
            chip,
            rings,
            info,
            interrupt,
            clock,
            link: Link::default(),
            looked: None,
            down_looks: 0,
            stalled_since: None,
            sent: 0,
            rdma_fix,
            stopped: false,
        }
    }

    /// Looks at the link once a second: at the PHY while it is down, and
    /// when the chip says it changed, setting the MAC up for it as
    /// FreeBSD's `bge_miibus_statchg()` does; has the PHY negotiate again
    /// after a while without one.
    fn watch_link(&mut self) {
        let now = (self.clock)();
        if self.looked.is_some_and(|then| now.wrapping_sub(then) < LINK_EVERY_NS) {
            return;
        }
        self.looked = Some(now);
        let (word, _) = self.rings.take_status();
        let changed = self.regs.read32(reg::MAC_STS) & reg::mac_sts::LINK_CHANGED != 0
            || word & rings::STATUS_LINK_CHANGED != 0;
        if self.link.up && !changed {
            return;
        }
        let mut delay = spin(self.clock);
        let link = Phy::new(&self.regs, &self.chip, &mut delay).link();
        self.regs.write32(reg::MAC_STS, reg::mac_sts::CHANGES);
        if link == self.link {
            if !link.up {
                self.down_looks += 1;
                if self.down_looks >= RENEGOTIATE_AFTER {
                    self.down_looks = 0;
                    Phy::new(&self.regs, &self.chip, &mut delay).negotiate();
                }
            }
            return;
        }
        self.link = link;
        self.down_looks = 0;
        if link.up {
            init::set_link(&self.regs, link, &mut delay);
        }
    }

    /// Notes how the chip sends: once it sent more frames than it has
    /// read DMA channels, the 5719's and 5720's fix is no longer needed,
    /// as FreeBSD's `bge_stats_update_regs()` finds; frames that wait
    /// with none sent start the clock [`Nic::failed`] reads.
    fn watch_sending(&mut self) {
        let (sent, waiting) = self.rings.progress();
        if sent != self.sent || !waiting {
            self.stalled_since = None;
        } else if self.stalled_since.is_none() {
            self.stalled_since = Some((self.clock)());
        }
        self.sent = sent;
        if self.rdma_fix && sent > reg::rdma::CHANNELS as u64 {
            let control = self.regs.read32(reg::RDMA_LSO_CRPTEN_CTRL);
            self.regs
                .write32(reg::RDMA_LSO_CRPTEN_CTRL, control & !init::tx_length_fix(&self.chip));
            self.rdma_fix = false;
        }
    }

    /// Tells the chip the driver has seen its status block, which lets
    /// it raise its MSI again: by the block's tag with tagged status,
    /// which has the chip raise it at once if it wrote a newer block.
    fn acknowledge(&self, tag: u32) {
        let value = if self.chip.tagged { tag << 24 } else { 0 };
        self.regs.write32(reg::MBX_IRQ0_LO, value);
    }
}

impl Nic for Card {
    fn info(&self) -> Info {
        self.info
    }

    fn link(&self) -> Link {
        self.link
    }

    fn failed(&self) -> bool {
        if self.regs.read32(reg::MAC_MODE) == u32::MAX || self.rings.broken() {
            return true;
        }
        // Frames that have waited too long, as FreeBSD's watchdog finds;
        // with pause frames off, the link partner cannot hold them.
        self.stalled_since.is_some_and(|since| (self.clock)().wrapping_sub(since) >= STALL_NS)
    }

    fn receive(&mut self, frame: &mut [u8]) -> Option<usize> {
        if self.stopped {
            return None;
        }
        // The tag the chip had written when no frame was left, read
        // first: a later block has a later tag.
        let tag = self.rings.tag();
        if let Some(len) = self.rings.receive(frame) {
            return Some(len);
        }
        self.watch_link();
        self.watch_sending();
        if self.interrupt == Interrupt::Msi {
            // Let the chip raise its MSI again, then look again for a
            // frame that came in between.
            self.acknowledge(tag);
            if self.rings.received() {
                return self.rings.receive(frame);
            }
        }
        None
    }

    fn can_send(&mut self) -> bool {
        !self.stopped && self.link.up && self.rings.can_send()
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        !self.stopped && self.link.up && self.rings.send(frame)
    }

    fn flush(&mut self) {
        if !self.stopped {
            self.rings.flush();
        }
    }

    fn dropped(&self) -> u64 {
        self.rings.dropped()
    }

    fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            init::stop(&self.regs, &self.chip, &mut spin(self.clock));
        }
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
