// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The DMA Remapping Reporting table (signature `DMAR`), which describes
//! an Intel machine's VT-d remapping units: where each one's registers
//! are, which PCI devices it translates, and which memory the firmware
//! keeps for devices' own use (Intel VT-d specification 4.1, chapter 8).

use crate::{Error, Sdt, u16_at, u64_at};

/// The parsed fixed part of the DMAR; [`Dmar::units`] and
/// [`Dmar::reserved`] walk its structures.
#[derive(Clone, Copy, Debug)]
pub struct Dmar<'a> {
    /// The widest physical address the machine's DMA can reach, in bits.
    pub host_address_width: u8,
    /// The flags byte ([`Dmar::interrupt_remapping`] and the others).
    pub flags: u8,
    structures: &'a [u8],
}

/// A DMA remapping unit (a DRHD structure): one set of VT-d registers
/// and the devices whose DMA goes through it.
#[derive(Clone, Copy, Debug)]
pub struct Unit<'a> {
    /// It translates every device on its segment that no other unit
    /// names (INCLUDE_PCI_ALL): its scopes then list only I/O APICs and
    /// HPETs.
    pub include_all: bool,
    /// The PCI segment its devices are on.
    pub segment: u16,
    /// The physical address of its registers.
    pub registers: u64,
    /// How many 4 KiB pages its registers take, at least one.
    pub register_pages: u32,
    scopes: &'a [u8],
}

/// Memory the firmware keeps for some devices' DMA (an RMRR structure),
/// such as a USB controller's legacy keyboard emulation or a graphics
/// controller's stolen memory: those devices must still reach it.
#[derive(Clone, Copy, Debug)]
pub struct Reserved<'a> {
    /// The PCI segment of its devices.
    pub segment: u16,
    /// Its first byte's physical address.
    pub base: u64,
    /// Its last byte's.
    pub limit: u64,
    scopes: &'a [u8],
}

/// What a device scope names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// A PCI function.
    Endpoint,
    /// A PCI bridge and every device below it.
    Bridge,
    /// An I/O APIC, by its ID.
    IoApic,
    /// An HPET block that sends MSIs, by its number.
    Hpet,
    /// A device of the ACPI namespace.
    Namespace,
    /// A kind the parser does not know.
    Other(u8),
}

/// A device a unit or a reserved region names: the bus to start at and
/// the path of (device, function) hops through bridges from there.
#[derive(Clone, Copy, Debug)]
pub struct DeviceScope<'a> {
    /// What it names.
    pub kind: ScopeKind,
    /// An I/O APIC's ID, an HPET's number, or an ACPI device's.
    pub enumeration_id: u8,
    /// The bus the path starts on.
    pub start_bus: u8,
    path: &'a [u8],
}

impl<'a> Dmar<'a> {
    /// The table's bytes after the header must hold the 12 fixed bytes.
    pub fn parse(sdt: &Sdt<'a>) -> Result<Dmar<'a>, Error> {
        if &sdt.signature != b"DMAR" {
            return Err(Error::BadSignature);
        }
        let body = sdt.body();
        let width = *body.first().ok_or(Error::TooShort)?;
        let flags = *body.get(1).ok_or(Error::TooShort)?;
        let structures = body.get(12..).ok_or(Error::TooShort)?;
        Ok(Dmar { host_address_width: width.saturating_add(1), flags, structures })
    }

    /// The firmware says the machine can remap interrupts (INTR_REMAP).
    pub fn interrupt_remapping(&self) -> bool {
        self.flags & 1 != 0
    }

    /// The firmware asks that x2APIC mode not be used (X2APIC_OPT_OUT).
    pub fn x2apic_opt_out(&self) -> bool {
        self.flags & 2 != 0
    }

    /// The firmware protected memory from DMA before boot and asks that it
    /// stay protected (DMA_CTRL_PLATFORM_OPT_IN).
    pub fn platform_opt_in(&self) -> bool {
        self.flags & 4 != 0
    }

    /// Its structures: (type, the whole structure, its 4-byte type and
    /// length included).
    fn structures(&self) -> impl Iterator<Item = (u16, &'a [u8])> + 'a {
        let mut rest = self.structures;
        core::iter::from_fn(move || {
            let kind = u16_at(rest, 0)?;
            let len = usize::from(u16_at(rest, 2)?);
            if len < 4 || len > rest.len() {
                return None;
            }
            let (this, after) = rest.split_at(len);
            rest = after;
            Some((kind, this))
        })
    }

    /// Its remapping units, in the table's order.
    pub fn units(&self) -> impl Iterator<Item = Unit<'a>> + 'a {
        self.structures().filter(|&(kind, _)| kind == 0).filter_map(|(_, bytes)| {
            let size = bytes.get(5)? & 0xf;
            Some(Unit {
                include_all: bytes.get(4)? & 1 != 0,
                segment: u16_at(bytes, 6)?,
                registers: u64_at(bytes, 8)?,
                register_pages: 1 << size,
                scopes: bytes.get(16..)?,
            })
        })
    }

    /// Its reserved memory regions, in the table's order.
    pub fn reserved(&self) -> impl Iterator<Item = Reserved<'a>> + 'a {
        self.structures().filter(|&(kind, _)| kind == 1).filter_map(|(_, bytes)| {
            let base = u64_at(bytes, 8)?;
            let limit = u64_at(bytes, 16)?;
            (limit >= base).then_some(Reserved {
                segment: u16_at(bytes, 6)?,
                base,
                limit,
                scopes: bytes.get(24..)?,
            })
        })
    }
}

/// Walks device scopes packed one after another.
fn scopes(mut rest: &[u8]) -> impl Iterator<Item = DeviceScope<'_>> {
    core::iter::from_fn(move || {
        let kind = *rest.first()?;
        let len = usize::from(*rest.get(1)?);
        if len < 6 || len > rest.len() {
            return None;
        }
        let (this, after) = rest.split_at(len);
        rest = after;
        let kind = match kind {
            1 => ScopeKind::Endpoint,
            2 => ScopeKind::Bridge,
            3 => ScopeKind::IoApic,
            4 => ScopeKind::Hpet,
            5 => ScopeKind::Namespace,
            other => ScopeKind::Other(other),
        };
        Some(DeviceScope { kind, enumeration_id: this[4], start_bus: this[5], path: &this[6..] })
    })
}

impl<'a> Unit<'a> {
    /// The devices it names.
    pub fn scopes(&self) -> impl Iterator<Item = DeviceScope<'a>> + 'a {
        scopes(self.scopes)
    }
}

impl<'a> Reserved<'a> {
    /// The devices that use it.
    pub fn scopes(&self) -> impl Iterator<Item = DeviceScope<'a>> + 'a {
        scopes(self.scopes)
    }
}

impl<'a> DeviceScope<'a> {
    /// The (device, function) hops from the start bus: through a bridge
    /// at each but the last, which is the device itself.
    pub fn path(&self) -> impl Iterator<Item = (u8, u8)> + 'a {
        self.path.as_chunks::<2>().0.iter().map(|hop| (hop[0], hop[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::table;

    fn scope(kind: u8, id: u8, bus: u8, path: &[(u8, u8)]) -> Vec<u8> {
        let mut bytes = vec![kind, (6 + 2 * path.len()) as u8, 0, 0, id, bus];
        for &(device, function) in path {
            bytes.extend_from_slice(&[device, function]);
        }
        bytes
    }

    fn structure(kind: u16, fixed: &[u8], scopes: &[Vec<u8>]) -> Vec<u8> {
        let len = 4 + fixed.len() + scopes.iter().map(Vec::len).sum::<usize>();
        let mut bytes = kind.to_le_bytes().to_vec();
        bytes.extend_from_slice(&(len as u16).to_le_bytes());
        bytes.extend_from_slice(fixed);
        for scope in scopes {
            bytes.extend_from_slice(scope);
        }
        bytes
    }

    /// A DMAR as a client machine's firmware writes one: a unit for the
    /// graphics controller alone, one for everything else, and reserved
    /// memory for the USB controller and the graphics controller.
    fn client() -> Vec<u8> {
        let mut body = vec![38, 0b101];
        body.extend_from_slice(&[0; 10]);
        let mut gfx = vec![0, 0];
        gfx.extend_from_slice(&0u16.to_le_bytes());
        gfx.extend_from_slice(&0xfed9_0000u64.to_le_bytes());
        body.extend(structure(0, &gfx, &[scope(1, 0, 0, &[(2, 0)])]));
        let mut all = vec![1, 1];
        all.extend_from_slice(&0u16.to_le_bytes());
        all.extend_from_slice(&0xfed9_1000u64.to_le_bytes());
        let ioapic = scope(3, 2, 0xf0, &[(0x1f, 0)]);
        body.extend(structure(0, &all, &[ioapic, scope(4, 0, 0, &[(0x1f, 7)])]));
        let mut usb = vec![0, 0];
        usb.extend_from_slice(&0u16.to_le_bytes());
        usb.extend_from_slice(&0x7c8a_0000u64.to_le_bytes());
        usb.extend_from_slice(&0x7c8b_ffffu64.to_le_bytes());
        body.extend(structure(1, &usb, &[scope(1, 0, 0, &[(0x14, 0)])]));
        // A structure of a kind the parser skips (an ATSR).
        body.extend(structure(2, &[0, 0, 0, 0], &[]));
        let mut gfx_memory = vec![0, 0];
        gfx_memory.extend_from_slice(&0u16.to_le_bytes());
        gfx_memory.extend_from_slice(&0x8b80_0000u64.to_le_bytes());
        gfx_memory.extend_from_slice(&0x8fff_ffffu64.to_le_bytes());
        body.extend(structure(1, &gfx_memory, &[scope(1, 0, 0, &[(0x1c, 0), (0, 0)])]));
        table(*b"DMAR", 1, &body)
    }

    #[test]
    fn units_and_reserved_memory_are_found() {
        let bytes = client();
        let dmar = Dmar::parse(&Sdt::parse(0x1000, &bytes).unwrap()).unwrap();
        assert_eq!(dmar.host_address_width, 39);
        assert!(dmar.interrupt_remapping());
        assert!(!dmar.x2apic_opt_out());
        assert!(dmar.platform_opt_in());
        let units: Vec<Unit<'_>> = dmar.units().collect();
        assert_eq!(units.len(), 2);
        assert!(!units[0].include_all);
        assert_eq!((units[0].registers, units[0].register_pages), (0xfed9_0000, 1));
        let gfx: Vec<DeviceScope<'_>> = units[0].scopes().collect();
        assert_eq!(gfx.len(), 1);
        assert_eq!((gfx[0].kind, gfx[0].start_bus), (ScopeKind::Endpoint, 0));
        assert_eq!(gfx[0].path().collect::<Vec<_>>(), [(2, 0)]);
        assert!(units[1].include_all);
        assert_eq!((units[1].registers, units[1].register_pages), (0xfed9_1000, 2));
        let kinds: Vec<(ScopeKind, u8)> =
            units[1].scopes().map(|s| (s.kind, s.enumeration_id)).collect();
        assert_eq!(kinds, [(ScopeKind::IoApic, 2), (ScopeKind::Hpet, 0)]);
        let reserved: Vec<Reserved<'_>> = dmar.reserved().collect();
        assert_eq!(reserved.len(), 2);
        assert_eq!((reserved[0].base, reserved[0].limit), (0x7c8a_0000, 0x7c8b_ffff));
        let behind_bridge: Vec<(u8, u8)> = reserved[1].scopes().next().unwrap().path().collect();
        assert_eq!(behind_bridge, [(0x1c, 0), (0, 0)]);
    }

    #[test]
    fn damaged_structures_end_the_walk() {
        let mut body = vec![47, 0];
        body.extend_from_slice(&[0; 10]);
        let mut unit = vec![1, 0];
        unit.extend_from_slice(&0u16.to_le_bytes());
        unit.extend_from_slice(&0xfed9_0000u64.to_le_bytes());
        body.extend(structure(0, &unit, &[]));
        // A structure that claims more bytes than the table has.
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&200u16.to_le_bytes());
        body.extend_from_slice(&[0; 12]);
        let bytes = table(*b"DMAR", 1, &body);
        let dmar = Dmar::parse(&Sdt::parse(0, &bytes).unwrap()).unwrap();
        assert_eq!(dmar.host_address_width, 48);
        assert_eq!(dmar.units().count(), 1);
        // A scope shorter than its fixed part ends its list.
        let mut bad = scope(1, 0, 0, &[(3, 0)]);
        bad[1] = 4;
        assert_eq!(scopes(&bad).count(), 0);
        let other = table(*b"APIC", 1, &[0; 12]);
        assert!(Dmar::parse(&Sdt::parse(0, &other).unwrap()).is_err());
    }
}
