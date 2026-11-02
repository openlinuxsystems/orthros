// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ethernet over USB (Phase 12, ADR 0199): the Ethernet Control Model of
//! the *Universal Serial Bus Communications Class Subclass Specification
//! for Ethernet Control Model Devices, Revision 1.2* ([`Protocol::Ecm`]),
//! which most USB Ethernet adapters, phones sharing their connection and
//! QEMU's `usb-net` speak, and the Network Control Model of the *USB
//! Network Control Model Devices Specification, Revision 1.0*
//! ([`Protocol::Ncm`]), which packs many frames into each transfer and
//! which the faster adapters and docks speak.
//!
//! Either way an adapter has a communications interface, whose
//! functional descriptors name its data interface and the string that
//! holds its MAC address, with an interrupt endpoint that says when its
//! link comes and goes ([`notification`]); and a data interface whose
//! first alternate setting has no endpoints, and whose second has the
//! bulk pair that carries the frames, which [`port`] finds in a
//! configuration. With ECM each transfer is a frame; with NCM each is a
//! transfer block of frames ([`ncm`]).
//!
//! Realtek's RTL8153 and the other adapters whose first configuration is
//! their maker's own have another that is ECM's or NCM's, which
//! [`port`] finds as well: the kernel puts them in that one.

use crate::TO_INTERFACE;
use crate::{CLASS, Configuration, DEVICE_TO_HOST, EndpointDescriptor, InterfaceDescriptor, Setup};
use crate::{TransferType, class};

/// The communications interface's subclasses (CDC 1.2, table 4).
pub mod subclass {
    /// The Ethernet Control Model.
    pub const ECM: u8 = 0x06;
    /// The Network Control Model.
    pub const NCM: u8 = 0x0d;
}

/// The class-specific descriptor type of an interface's functional
/// descriptors (CDC 1.2, table 12).
const CS_INTERFACE: u8 = 0x24;
/// Functional descriptor subtypes (CDC 1.2, table 13): the union, which
/// names the data interface, and the Ethernet networking descriptor.
const UNION: u8 = 0x06;
const ETHERNET: u8 = 0x0f;

/// Which protocol an adapter speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// ECM: a frame a transfer.
    Ecm,
    /// NCM: transfer blocks of frames.
    Ncm,
}

impl Protocol {
    /// Its name, for the log.
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Ecm => "CDC-ECM",
            Protocol::Ncm => "CDC-NCM",
        }
    }

    /// The name of Linux's driver for it, which /kernel/bus/usb shows as
    /// its interfaces' driver.
    pub fn driver(self) -> &'static str {
        match self {
            Protocol::Ecm => "cdc_ether",
            Protocol::Ncm => "cdc_ncm",
        }
    }
}

/// An adapter, as a configuration describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    /// The protocol it speaks.
    pub protocol: Protocol,
    /// The communications interface's number.
    pub control: u8,
    /// The data interface's number.
    pub data: u8,
    /// The data interface's alternate setting with the bulk endpoints.
    pub alternate: u8,
    /// The bulk endpoint that carries the frames it receives.
    pub bulk_in: EndpointDescriptor,
    /// The bulk endpoint that carries the frames it sends.
    pub bulk_out: EndpointDescriptor,
    /// The interrupt endpoint that says when the link changes, if it has
    /// one.
    pub notify: Option<EndpointDescriptor>,
    /// The string that holds its MAC address, 0 for none.
    pub mac_string: u8,
    /// The longest frame it takes, with its header, as it says.
    pub max_segment: u16,
}

/// The adapter in `config`, if it has one: its first ECM or NCM
/// communications interface whose data interface has a bulk pair.
pub fn port(config: &Configuration<'_>) -> Option<Port> {
    config.interfaces().find_map(|interface| {
        let d = interface.descriptor;
        let protocol = match (d.class, d.subclass) {
            (class::COMMUNICATIONS, subclass::ECM) => Protocol::Ecm,
            (class::COMMUNICATIONS, subclass::NCM) => Protocol::Ncm,
            _ => return None,
        };
        if d.alternate != 0 {
            return None;
        }
        let mut data = None;
        let mut mac_string = 0;
        let mut max_segment = 1514;
        for (kind, bytes) in interface.descriptors() {
            if kind != CS_INTERFACE || bytes.len() < 3 {
                continue;
            }
            match bytes[2] {
                UNION if bytes.len() >= 5 => data = data.or(Some(bytes[4])),
                ETHERNET if bytes.len() >= 13 => {
                    mac_string = bytes[3];
                    max_segment = u16::from_le_bytes([bytes[8], bytes[9]]);
                }
                _ => {}
            }
        }
        // Without a union, the data interface is the next one, as Linux
        // guesses.
        let data = data.unwrap_or(d.number.wrapping_add(1));
        let notify = interface
            .endpoints()
            .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt);
        let (alternate, bulk_in, bulk_out) = bulk_setting(config, data)?;
        Some(Port {
            protocol,
            control: d.number,
            data,
            alternate,
            bulk_in,
            bulk_out,
            notify,
            mac_string,
            max_segment,
        })
    })
}

/// Whether a configuration whose first interface is `first` is passed
/// over for another that has an adapter: one of its maker's own, as
/// Realtek's RTL8153 has first, or Microsoft's RNDIS or ActiveSync, as
/// QEMU's `usb-net` has. Linux chooses so too, for want of their drivers.
pub fn passed_over(first: &InterfaceDescriptor) -> bool {
    match (first.class, first.subclass, first.protocol) {
        (class::VENDOR, ..) => true,
        // RNDIS, on a communications interface or a wireless one.
        (class::COMMUNICATIONS, 0x02, 0xff) | (class::WIRELESS, 0x01, 0x03) => true,
        // ActiveSync.
        (class::MISCELLANEOUS, 0x01, 0x01) => true,
        _ => false,
    }
}

/// The alternate setting of interface `number` that has a bulk endpoint
/// each way, and the two.
fn bulk_setting(
    config: &Configuration<'_>,
    number: u8,
) -> Option<(u8, EndpointDescriptor, EndpointDescriptor)> {
    config.interfaces().filter(|i| i.descriptor.number == number).find_map(|interface| {
        if interface.descriptor.class != class::CDC_DATA {
            return None;
        }
        let bulk = |into: bool| {
            interface
                .endpoints()
                .find(|e| e.transfer_type() == TransferType::Bulk && e.is_in() == into)
        };
        let (input, output) = (bulk(true)?, bulk(false)?);
        (input.max_packet() > 0 && output.max_packet() > 0).then_some((
            interface.descriptor.alternate,
            input,
            output,
        ))
    })
}

/// The MAC address in the string descriptor `bytes`: twelve hex digits,
/// as ECM 1.2's table 3 has it. `None` if it is not one a card can have.
pub fn mac_from_string(bytes: &[u8]) -> Option<[u8; 6]> {
    if bytes.len() < 2 || bytes[1] != crate::descriptor::STRING {
        return None;
    }
    let len = bytes.len().min(usize::from(bytes[0]));
    let units = bytes.get(2..len)?.as_chunks::<2>().0;
    if units.len() < 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, pair) in units[..12].iter().enumerate() {
        let unit = u16::from_le_bytes(*pair);
        let digit = char::from_u32(u32::from(unit))?.to_digit(16)? as u8;
        mac[i / 2] = mac[i / 2] << 4 | digit;
    }
    let usable = mac != [0; 6] && mac != [0xff; 6] && mac[0] & 0x01 == 0;
    usable.then_some(mac)
}

/// The packet filter's bits (ECM 1.2, table 8): every multicast frame,
/// the adapter's own and broadcasts.
pub mod filter {
    /// Every frame on the wire.
    pub const PROMISCUOUS: u16 = 0x01;
    /// Every multicast frame.
    pub const ALL_MULTICAST: u16 = 0x02;
    /// Frames to the adapter's own address.
    pub const DIRECTED: u16 = 0x04;
    /// Broadcasts.
    pub const BROADCAST: u16 = 0x08;
}

/// The class requests (CDC 1.2, table 19; NCM 1.0, table 6-2).
mod request {
    pub const SET_ETHERNET_PACKET_FILTER: u8 = 0x43;
    pub const GET_NTB_PARAMETERS: u8 = 0x80;
    pub const SET_NTB_INPUT_SIZE: u8 = 0x86;
}

/// SET_ETHERNET_PACKET_FILTER to the communications interface `control`:
/// which frames the adapter passes on ([`filter`]).
pub fn set_packet_filter(control: u8, bits: u16) -> Setup {
    Setup {
        request_type: CLASS | TO_INTERFACE,
        request: request::SET_ETHERNET_PACKET_FILTER,
        value: bits,
        index: u16::from(control),
        length: 0,
    }
}

/// GET_NTB_PARAMETERS from the NCM communications interface `control`:
/// [`ncm::PARAMETERS`] bytes of [`ncm::Parameters`].
pub fn get_ntb_parameters(control: u8) -> Setup {
    Setup {
        request_type: DEVICE_TO_HOST | CLASS | TO_INTERFACE,
        request: request::GET_NTB_PARAMETERS,
        value: 0,
        index: u16::from(control),
        length: ncm::PARAMETERS as u16,
    }
}

/// SET_NTB_INPUT_SIZE to the NCM communications interface `control`, with
/// its four bytes, the longest transfer block it may send, in the data
/// stage ([`ntb_input_size`]).
pub fn set_ntb_input_size(control: u8) -> Setup {
    Setup {
        request_type: CLASS | TO_INTERFACE,
        request: request::SET_NTB_INPUT_SIZE,
        value: 0,
        index: u16::from(control),
        length: 4,
    }
}

/// The data stage of [`set_ntb_input_size`]: `size` bytes.
pub fn ntb_input_size(size: u32) -> [u8; 4] {
    size.to_le_bytes()
}

/// What the interrupt endpoint says (CDC 1.2, 6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    /// The link is up, or down (NETWORK_CONNECTION).
    Connection(bool),
    /// The link's speeds, down and up, in bits a second
    /// (CONNECTION_SPEED_CHANGE).
    Speed(u32, u32),
}

/// The notification in `bytes`, as the interrupt endpoint sent it, if it
/// is one of [`Notification`]'s.
pub fn notification(bytes: &[u8]) -> Option<Notification> {
    if bytes.len() < 8 || bytes[0] != DEVICE_TO_HOST | CLASS | TO_INTERFACE {
        return None;
    }
    match bytes[1] {
        0x00 => Some(Notification::Connection(u16::from_le_bytes([bytes[2], bytes[3]]) != 0)),
        0x2a if bytes.len() >= 16 => {
            let word = |at: usize| {
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            Some(Notification::Speed(word(8), word(12)))
        }
        _ => None,
    }
}

/// NCM's transfer blocks with 16-bit offsets (NCM 1.0, section 3): a
/// header (NTH16), and a datagram pointer table (NDP16) that lists the
/// frames in the block.
pub mod ncm {
    /// The bytes of GET_NTB_PARAMETERS's answer.
    pub const PARAMETERS: usize = 28;
    /// NTH16's signature, "NCMH", its length, and NDP16's signature
    /// without a CRC, "NCM0".
    const NTH16: u32 = 0x484d_434e;
    const NTH16_LENGTH: usize = 12;
    const NDP16: u32 = 0x304d_434e;
    /// The bytes of an NDP16 with one frame and the entry that ends the
    /// list.
    const NDP16_ONE: usize = 8 + 2 * 4;
    /// The formats bit for 16-bit blocks, which every adapter has.
    pub const FORMAT_16: u16 = 0x0001;

    /// What an adapter says of its transfer blocks (NCM 1.0, table 6-3).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Parameters {
        /// The longest block it sends.
        pub in_max: u32,
        /// The longest block it takes.
        pub out_max: u32,
        /// Where a frame it takes may start, in a block: at
        /// [`Parameters::out_remainder`] past a multiple of this.
        pub out_divisor: u16,
        /// See [`Parameters::out_divisor`].
        pub out_remainder: u16,
        /// The alignment of a datagram pointer table it takes.
        pub out_alignment: u16,
    }

    impl Parameters {
        /// Reads them from GET_NTB_PARAMETERS's answer.
        pub fn parse(bytes: &[u8]) -> Option<Parameters> {
            if bytes.len() < PARAMETERS {
                return None;
            }
            let half = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            let word = |at: usize| {
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            if half(2) & FORMAT_16 == 0 {
                return None;
            }
            let parameters = Parameters {
                in_max: word(4),
                out_max: word(16),
                out_divisor: half(20).max(1),
                out_remainder: half(22),
                out_alignment: half(24).max(4),
            };
            (parameters.out_alignment.is_power_of_two()
                && parameters.out_remainder < parameters.out_divisor)
                .then_some(parameters)
        }

        /// The bytes of the block that carries a frame of `len` bytes.
        pub fn block_length(&self, len: usize) -> usize {
            self.frame_at() + len
        }

        /// Where a block's frame starts: after its header and its table,
        /// where the adapter wants frames to start.
        fn frame_at(&self) -> usize {
            let after = NTH16_LENGTH.next_multiple_of(usize::from(self.out_alignment)) + NDP16_ONE;
            let divisor = usize::from(self.out_divisor);
            let remainder = usize::from(self.out_remainder);
            let mut at = after.next_multiple_of(divisor) + remainder;
            if at < after {
                at += divisor;
            }
            at
        }
    }

    /// Writes into `out` a block of sequence number `sequence` that
    /// carries `frame`, and says how long it is: `None` if it does not
    /// fit in `out`, or is longer than the adapter takes.
    pub fn build(
        parameters: &Parameters,
        sequence: u16,
        frame: &[u8],
        out: &mut [u8],
    ) -> Option<usize> {
        let at = parameters.frame_at();
        let len = at + frame.len();
        if len > out.len() || len > parameters.out_max as usize || len > usize::from(u16::MAX) {
            return None;
        }
        let table = NTH16_LENGTH.next_multiple_of(usize::from(parameters.out_alignment));
        out[..at].fill(0);
        out[0..4].copy_from_slice(&NTH16.to_le_bytes());
        out[4..6].copy_from_slice(&(NTH16_LENGTH as u16).to_le_bytes());
        out[6..8].copy_from_slice(&sequence.to_le_bytes());
        out[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        out[10..12].copy_from_slice(&(table as u16).to_le_bytes());
        out[table..table + 4].copy_from_slice(&NDP16.to_le_bytes());
        out[table + 4..table + 6].copy_from_slice(&(NDP16_ONE as u16).to_le_bytes());
        // No next table; the frame; the entry of zeros that ends the list.
        out[table + 8..table + 10].copy_from_slice(&(at as u16).to_le_bytes());
        out[table + 10..table + 12].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        out[at..len].copy_from_slice(frame);
        Some(len)
    }

    /// The frames in the block `block`, as (offset, length) in it: none if
    /// it is not a block, and only those wholly inside it. At most the
    /// first 64 tables are read, which a block from a broken adapter
    /// could otherwise chain without end.
    pub fn frames(block: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
        let half = |at: usize| -> Option<usize> {
            block.get(at..at + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        };
        let word = |at: usize| -> Option<u32> {
            block.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let header = (word(0) == Some(NTH16) && half(4) == Some(NTH16_LENGTH))
            .then(|| {
                // A block shorter than it says came cut, and is dropped.
                let len = half(8)?;
                (NTH16_LENGTH..=block.len()).contains(&len).then_some((len, half(10)?))
            })
            .flatten();
        let (len, mut table) = header.unwrap_or((0, 0));
        let block = &block[..len];
        let mut tables = 0;
        let mut entry = 0;
        core::iter::from_fn(move || {
            loop {
                if table == 0 || tables >= 64 || table + 8 > block.len() {
                    return None;
                }
                let signature = word(table);
                let length = usize::from(u16::from_le_bytes([block[table + 4], block[table + 5]]));
                // "NCM0", or "NCM1" for frames with a CRC after them, which
                // is not asked for and so is not taken.
                if signature != Some(NDP16) || length < 16 || table + length > block.len() {
                    return None;
                }
                let at = table + 8 + entry * 4;
                if at + 4 > table + length {
                    tables += 1;
                    entry = 0;
                    table = usize::from(u16::from_le_bytes([block[table + 6], block[table + 7]]));
                    continue;
                }
                let offset = usize::from(u16::from_le_bytes([block[at], block[at + 1]]));
                let frame = usize::from(u16::from_le_bytes([block[at + 2], block[at + 3]]));
                entry += 1;
                if offset == 0 || frame == 0 {
                    tables += 1;
                    entry = 0;
                    table = usize::from(u16::from_le_bytes([block[table + 6], block[table + 7]]));
                    continue;
                }
                if offset.checked_add(frame).is_some_and(|end| end <= block.len()) {
                    return Some((offset, frame));
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
