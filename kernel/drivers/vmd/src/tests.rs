// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The plan against a simulated domain: root ports, a switch, NVMe disks
//! and an AHCI controller, whose BARs and bridge registers behave as the
//! PCI specifications say, and whose configuration space answers only
//! where the bridges' bus numbers lead.

use super::*;

/// A BAR as the hardware has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spec {
    None,
    /// A 32-bit memory BAR of `size` bytes.
    Memory32(u64),
    /// A 64-bit memory BAR (and the register after it).
    Memory64 {
        size: u64,
        prefetchable: bool,
    },
    /// The upper half of a 64-bit one.
    Upper,
    /// I/O ports.
    Io(u32),
}

struct Dev {
    parent: Option<usize>,
    slot: (u8, u8),
    id: u32,
    header: u8,
    bars: [Spec; 6],
    /// A bridge's prefetchable window has 64-bit addresses.
    wide_prefetchable: bool,
    regs: [u32; 64],
}

/// A window's base and size, if it is open.
type Span = Option<(u64, u64)>;

struct Sim {
    first_bus: u8,
    devs: Vec<Dev>,
}

impl Sim {
    fn new(first_bus: u8) -> Sim {
        Sim { first_bus, devs: Vec::new() }
    }

    fn add(&mut self, parent: Option<usize>, slot: (u8, u8), id: u32, bars: [Spec; 6]) -> usize {
        self.devs.push(Dev {
            parent,
            slot,
            id,
            header: 0,
            bars,
            wide_prefetchable: false,
            regs: [0; 64],
        });
        self.devs.len() - 1
    }

    fn bridge(&mut self, parent: Option<usize>, slot: (u8, u8), wide_prefetchable: bool) -> usize {
        let index = self.add(parent, slot, 0x9a0b_8086, [Spec::None; 6]);
        let dev = &mut self.devs[index];
        dev.header = 1;
        dev.wide_prefetchable = wide_prefetchable;
        // As firmware might leave it: windows open at zero.
        dev.regs[usize::from(reg::PREFETCHABLE_BASE) / 4] =
            if wide_prefetchable { 0x0001_0001 } else { 0 };
        index
    }

    /// The bus `index` is on, if configuration cycles reach it.
    fn bus_of(&self, index: usize) -> Option<u8> {
        match self.devs[index].parent {
            None => Some(self.first_bus),
            Some(parent) => {
                self.bus_of(parent)?;
                let secondary = (self.devs[parent].regs[6] >> 8) as u8;
                (secondary != 0).then_some(secondary)
            }
        }
    }

    fn find(&self, at: Location) -> Option<usize> {
        (0..self.devs.len()).find(|&index| {
            self.devs[index].slot == (at.device, at.function) && self.bus_of(index) == Some(at.bus)
        })
    }

    fn reg(&self, index: usize, offset: u16) -> u32 {
        self.devs[index].regs[usize::from(offset) / 4]
    }

    /// Where BAR `bar` of `index` points, and its size.
    fn bar(&self, index: usize, bar: usize) -> Option<(u64, u64)> {
        let dev = &self.devs[index];
        let low = u64::from(dev.regs[4 + bar] & !0xf);
        match dev.bars[bar] {
            Spec::Memory32(size) => Some((low, size)),
            Spec::Memory64 { size, .. } => {
                Some(((u64::from(dev.regs[4 + bar + 1]) << 32) | low, size))
            }
            _ => None,
        }
    }

    /// A bridge's memory window and prefetchable window, if open.
    fn windows(&self, index: usize) -> (Span, Span) {
        let memory = self.reg(index, reg::MEMORY_BASE);
        let base = u64::from(memory & 0xfff0) << 16;
        let limit = (u64::from(memory >> 16 & 0xfff0) << 16) | 0xf_ffff;
        let memory = (base <= limit).then(|| (base, limit + 1 - base));
        let low = self.reg(index, reg::PREFETCHABLE_BASE);
        let base = (u64::from(self.reg(index, reg::PREFETCHABLE_BASE_UPPER)) << 32)
            | (u64::from(low & 0xfff0) << 16);
        let limit = (u64::from(self.reg(index, reg::PREFETCHABLE_LIMIT_UPPER)) << 32)
            | (u64::from(low >> 16 & 0xfff0) << 16)
            | 0xf_ffff;
        let prefetchable = (base <= limit).then(|| (base, limit + 1 - base));
        (memory, prefetchable)
    }

    fn command(&self, index: usize) -> u16 {
        self.reg(index, reg::COMMAND) as u16
    }

    fn write(&mut self, index: usize, offset: u16, value: u32) {
        let dev = &mut self.devs[index];
        let word = usize::from(offset) / 4;
        match offset {
            reg::COMMAND => dev.regs[word] = (dev.regs[word] & 0xffff_0000) | (value & 0x07ff),
            0x10..=0x24 if dev.header & 0x7f == 0 || offset < 0x18 => {
                let bar = word - 4;
                dev.regs[word] = match dev.bars[bar] {
                    Spec::None => 0,
                    Spec::Memory32(size) => value & !(size as u32 - 1) & !0xf,
                    Spec::Memory64 { size, prefetchable } => {
                        let low = value & !((size - 1) as u32) & !0xf;
                        low | 0x4 | if prefetchable { 0x8 } else { 0 }
                    }
                    Spec::Upper => {
                        let Spec::Memory64 { size, .. } = dev.bars[bar - 1] else { unreachable!() };
                        value & !((size - 1) >> 32) as u32
                    }
                    Spec::Io(size) => (value & !(size - 1) & !0x3) | 1,
                };
            }
            reg::BUS_NUMBERS => dev.regs[word] = value,
            reg::MEMORY_BASE => dev.regs[word] = value & 0xfff0_fff0,
            reg::PREFETCHABLE_BASE => {
                let kind = if dev.wide_prefetchable { 0x0001_0001 } else { 0 };
                dev.regs[word] = (value & 0xfff0_fff0) | kind;
            }
            reg::PREFETCHABLE_BASE_UPPER | reg::PREFETCHABLE_LIMIT_UPPER => {
                if dev.wide_prefetchable {
                    dev.regs[word] = value;
                }
            }
            reg::IO_UPPER => dev.regs[word] = value,
            _ => {}
        }
    }
}

impl Config for Sim {
    fn read32(&mut self, at: Location, offset: u16) -> u32 {
        let Some(index) = self.find(at) else { return u32::MAX };
        let dev = &self.devs[index];
        match offset {
            reg::ID => dev.id,
            reg::HEADER => u32::from(dev.header) << 16,
            0x10..=0x24 if dev.header & 0x7f == 0 || offset < 0x18 => {
                let bar = usize::from(offset - 0x10) / 4;
                let value = dev.regs[usize::from(offset) / 4];
                match dev.bars[bar] {
                    // Before any write, a BAR reads its type.
                    Spec::Memory64 { prefetchable, .. } => {
                        (value & !0xf) | 0x4 | if prefetchable { 0x8 } else { 0 }
                    }
                    Spec::Io(_) => value | 1,
                    _ => value,
                }
            }
            _ => dev.regs[usize::from(offset) / 4],
        }
    }

    fn write32(&mut self, at: Location, offset: u16, value: u32) {
        if let Some(index) = self.find(at) {
            self.write(index, offset, value);
        }
    }

    fn write16(&mut self, at: Location, offset: u16, value: u16) {
        let Some(index) = self.find(at) else { return };
        if offset == reg::IO_BASE {
            let word = usize::from(offset) / 4;
            let dev = &mut self.devs[index];
            dev.regs[word] = (dev.regs[word] & 0xffff_0000) | u32::from(value);
        } else {
            assert_eq!(offset, reg::COMMAND, "16-bit writes go to the command register");
            self.write(index, offset, u32::from(value));
        }
    }
}

const NVME: [Spec; 6] = [
    Spec::Memory64 { size: 0x4000, prefetchable: false },
    Spec::Upper,
    Spec::None,
    Spec::None,
    Spec::None,
    Spec::None,
];

const MIB: u64 = 1 << 20;

/// A client VMD's domain: buses from 224, 32 MiB of MEMBAR1 below 4 GiB
/// and 32 MiB of MEMBAR2 above.
fn client() -> Domain {
    Domain::new(
        224,
        32 * MIB,
        Range { base: 0x8000_0000, size: 32 * MIB },
        Range { base: 0x40_0000_0000, size: 32 * MIB },
    )
    .unwrap()
}

/// Every BAR placed is aligned to its size, inside its bridge's window if
/// it has one (and the windows inside their bridges'), and none overlap.
fn check_layout(sim: &Sim, domain: &Domain) {
    let mut placed = Vec::new();
    for index in 0..sim.devs.len() {
        if sim.bus_of(index).is_none() {
            continue;
        }
        for bar in 0..6 {
            let Some((base, size)) = sim.bar(index, bar) else { continue };
            assert_eq!(base % size, 0, "BAR {bar} of device {index} is aligned");
            let prefetchable =
                matches!(sim.devs[index].bars[bar], Spec::Memory64 { prefetchable: true, .. });
            let inside = |(start, len): (u64, u64)| base >= start && base + size <= start + len;
            match sim.devs[index].parent {
                None => {
                    let window = if prefetchable && base >= domain.prefetchable.base {
                        domain.prefetchable
                    } else {
                        domain.memory
                    };
                    assert!(inside((window.base, window.size)), "{base:#x} is in its window");
                }
                Some(parent) => {
                    let (memory, wide) = sim.windows(parent);
                    let ok = memory.is_some_and(inside) || wide.is_some_and(inside);
                    assert!(ok, "{base:#x} is in its bridge's window");
                }
            }
            placed.push((base, size));
        }
        if sim.devs[index].header & 0x7f == 1
            && let Some(parent) = sim.devs[index].parent
        {
            let (outer_memory, outer_wide) = sim.windows(parent);
            let (memory, wide) = sim.windows(index);
            let within = |inner: Span, outer: Span| {
                inner.is_none_or(|(b, s)| outer.is_some_and(|(ob, os)| b >= ob && b + s <= ob + os))
            };
            assert!(within(memory, outer_memory), "bridge {index}'s window is in its bridge's");
            assert!(within(wide, outer_wide), "bridge {index}'s prefetchable window too");
        }
    }
    placed.sort_unstable();
    for pair in placed.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:x?} do not overlap");
    }
}

#[test]
fn a_laptop_s_domain_is_numbered_and_placed() {
    let mut sim = Sim::new(224);
    // Two root ports, functions 0 and 2 of one device.
    let first = sim.bridge(None, (6, 0), true);
    sim.devs[first].header = 0x81;
    let disk = sim.add(Some(first), (0, 0), 0x5411_144d, NVME);
    let second = sim.bridge(None, (6, 2), true);
    let wide = [
        Spec::Memory64 { size: 0x4000, prefetchable: false },
        Spec::Upper,
        Spec::Memory64 { size: 2 * MIB, prefetchable: true },
        Spec::Upper,
        Spec::Io(0x100),
        Spec::None,
    ];
    let other = sim.add(Some(second), (0, 0), 0x2263_c0a9, wide);
    let ahci =
        [Spec::Io(8), Spec::Io(4), Spec::Io(8), Spec::Io(4), Spec::Io(32), Spec::Memory32(0x800)];
    let sata = sim.add(None, (23, 0), 0x7ae2_8086, ahci);
    let domain = client();
    let mut plan = Plan::new();
    let summary = plan.configure(&mut sim, &domain).unwrap();
    assert_eq!(summary.bridges, 2);
    assert_eq!(summary.endpoints, 3);
    assert_eq!(summary.io_bars, 6);
    assert_eq!(summary.unnumbered, 0);
    assert_eq!(summary.last_bus, 226);
    assert_eq!(sim.reg(first, reg::BUS_NUMBERS) & 0xff_ffff, 0xe1_e1e0);
    assert_eq!(sim.reg(second, reg::BUS_NUMBERS) & 0xff_ffff, 0xe2_e2e0);
    // The disks are reached on the buses behind their ports.
    assert_eq!(sim.bus_of(disk), Some(225));
    assert_eq!(sim.bus_of(other), Some(226));
    // A megabyte window for each port, the AHCI's 2 KiB after them.
    assert_eq!(summary.memory_used, 2 * MIB + 0x800);
    let (memory, wide_window) = sim.windows(first);
    assert_eq!(memory.unwrap().1, MIB);
    assert_eq!(wide_window, None);
    let (_, wide_window) = sim.windows(second);
    let (base, size) = wide_window.unwrap();
    assert!(base >= domain.prefetchable.base, "past the VMD's MSI-X table");
    assert_eq!(size, 2 * MIB);
    assert_eq!(sim.bar(other, 2), Some((base, 2 * MIB)));
    // The I/O BARs are left at zero, and I/O decoding off.
    assert_eq!(sim.reg(other, reg::BAR0 + 16) & !0x3, 0);
    for index in [first, second] {
        assert_eq!(sim.command(index), command::MEMORY | command::BUS_MASTER);
    }
    for index in [disk, other, sata] {
        assert_eq!(sim.command(index), command::MEMORY, "drivers turn DMA on");
    }
    check_layout(&sim, &domain);
}

#[test]
fn a_switch_s_ports_are_numbered_depth_first() {
    let mut sim = Sim::new(0);
    let port = sim.bridge(None, (0, 0), true);
    let upstream = sim.bridge(Some(port), (0, 0), true);
    let down = [sim.bridge(Some(upstream), (0, 0), true), sim.bridge(Some(upstream), (1, 0), true)];
    let disks = [sim.add(Some(down[0]), (0, 0), 1, NVME), sim.add(Some(down[1]), (0, 0), 2, NVME)];
    let after = sim.bridge(None, (1, 0), true);
    let last = sim.add(Some(after), (0, 0), 3, NVME);
    let domain = Domain::new(
        0,
        256 * MIB,
        Range { base: 0xa000_0000, size: 64 * MIB },
        Range { base: 0xb000_0000, size: 64 * MIB },
    )
    .unwrap();
    let summary = Plan::new().configure(&mut sim, &domain).unwrap();
    assert_eq!(summary.bridges, 5);
    assert_eq!(summary.last_bus, 5);
    let numbers = |index| sim.reg(index, reg::BUS_NUMBERS) & 0xff_ffff;
    assert_eq!(numbers(port), 0x04_0100);
    assert_eq!(numbers(upstream), 0x04_0201);
    assert_eq!(numbers(down[0]), 0x03_0302);
    assert_eq!(numbers(down[1]), 0x04_0402);
    assert_eq!(numbers(after), 0x05_0500);
    assert_eq!(sim.bus_of(disks[0]), Some(3));
    assert_eq!(sim.bus_of(disks[1]), Some(4));
    assert_eq!(sim.bus_of(last), Some(5));
    // The switch's window holds both of its ports' megabytes.
    assert_eq!(sim.windows(upstream).0.unwrap().1, 2 * MIB);
    check_layout(&sim, &domain);
}

#[test]
fn prefetchable_bars_behind_a_narrow_bridge_go_below_4g() {
    let mut sim = Sim::new(0);
    let port = sim.bridge(None, (0, 0), false);
    let bars = [
        Spec::Memory64 { size: 4 * MIB, prefetchable: true },
        Spec::Upper,
        Spec::None,
        Spec::None,
        Spec::None,
        Spec::None,
    ];
    let device = sim.add(Some(port), (0, 0), 1, bars);
    let domain = client_from(0);
    Plan::new().configure(&mut sim, &domain).unwrap();
    let (base, _) = sim.bar(device, 0).unwrap();
    assert!(base >= domain.memory.base && base < domain.memory.end());
    assert_eq!(base % (4 * MIB), 0);
    check_layout(&sim, &domain);
}

fn client_from(first_bus: u8) -> Domain {
    Domain { first_bus, last_bus: first_bus + 31, ..client() }
}

#[test]
fn too_little_room_is_an_error() {
    let mut sim = Sim::new(0);
    let bars =
        [Spec::Memory32(64 * MIB), Spec::None, Spec::None, Spec::None, Spec::None, Spec::None];
    sim.add(None, (0, 0), 1, bars);
    let err = Plan::new().configure(&mut sim, &client_from(0)).unwrap_err();
    assert_eq!(err, Error::NoRoom { prefetchable: false, needed: 64 * MIB, room: 32 * MIB });
    assert_eq!(
        err.to_string(),
        "its devices need 65536 KiB of memory and its window has 32768 KiB"
    );
}

#[test]
fn bridges_past_the_last_bus_are_left_unnumbered() {
    let mut sim = Sim::new(254);
    let ports = [
        sim.bridge(None, (0, 0), true),
        sim.bridge(None, (1, 0), true),
        sim.bridge(None, (2, 0), true),
    ];
    let disk = sim.add(Some(ports[0]), (0, 0), 1, NVME);
    let lost = sim.add(Some(ports[1]), (0, 0), 2, NVME);
    let domain =
        Domain::new(254, 2 * MIB, Range { base: 0x8000_0000, size: 32 * MIB }, Range::default())
            .unwrap();
    assert_eq!(domain.last_bus, 255);
    let summary = Plan::new().configure(&mut sim, &domain).unwrap();
    assert_eq!(summary.unnumbered, 2);
    assert_eq!(summary.last_bus, 255);
    assert_eq!(sim.bus_of(disk), Some(255));
    assert_eq!(sim.bus_of(lost), None);
    assert_eq!(summary.prefetchable_used, 0);
    check_layout(&sim, &domain);
}

#[test]
fn the_domain_and_its_buses_are_read_from_the_vmd() {
    assert_eq!(first_bus(false, 1, 0x200), Ok(0));
    assert_eq!(first_bus(true, 0, 0x200), Ok(0));
    assert_eq!(first_bus(true, 1, 0x000), Ok(0));
    assert_eq!(first_bus(true, 1, 0x100), Ok(128));
    assert_eq!(first_bus(true, 1, 0x202), Ok(224));
    assert_eq!(first_bus(true, 1, 0x300), Err(Error::BusRange));
    assert_eq!(model(0x8086, 0x9a0b), Some(true));
    assert_eq!(model(0x8086, 0x201d), Some(false));
    assert_eq!(model(0x8086, 0x9a09), None);
    assert_eq!(model(0x1022, 0x9a0b), None);
    let membar1 = Range { base: 0x8000_0000, size: 32 * MIB };
    let membar2 = Range { base: 0x40_0000_0000, size: 32 * MIB };
    let domain = Domain::new(128, 64 * MIB, membar1, membar2).unwrap();
    assert_eq!((domain.first_bus, domain.last_bus), (128, 191));
    assert_eq!(domain.prefetchable.base, 0x40_0000_2010);
    assert_eq!(domain.prefetchable.end(), membar2.end());
    assert_eq!(Domain::new(224, 256 * MIB, membar1, membar2).unwrap().last_bus, 255);
    assert_eq!(Domain::new(0, 0, membar1, membar2), Err(Error::NoConfigSpace));
    let high = Range { base: 0x1_0000_0000, size: MIB };
    assert_eq!(Domain::new(0, MIB, high, membar2), Err(Error::MemoryAbove4G));
    let small = Range { base: 0xc000_0000, size: 0x2000 };
    assert_eq!(Domain::new(0, MIB, membar1, small).unwrap().prefetchable, Range::default());
}

#[test]
fn every_function_s_decoding_is_off_while_it_is_sized() {
    // A device whose firmware left it decoding: the walk turns it off
    // first, and on again only once its BAR is placed.
    let mut sim = Sim::new(0);
    let device = sim.add(None, (4, 0), 1, NVME);
    sim.devs[device].regs[1] = u32::from(command::MEMORY | command::BUS_MASTER | command::IO);
    let domain = client_from(0);
    Plan::new().configure(&mut sim, &domain).unwrap();
    assert_eq!(sim.command(device), command::MEMORY);
    assert_eq!(sim.bar(device, 0), Some((domain.memory.base, 0x4000)));
}
