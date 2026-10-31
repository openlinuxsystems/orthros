// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the network capability (Phase 12, radix-network(2),
//! ADR 0183): making network sockets, deriving a capability for fewer
//! ports or rights, as Spark does for each service, and with the control
//! right setting the interfaces up (ADR 0190).
//!
//! What a network capability allows is checked again where it is used:
//! by the socket methods that take its slot (`files`), not by the socket
//! it made, which is a file like any other.

use radix_abi::errors::{File, Kernel};
use radix_abi::network::{
    self as abi, AddressFamily, ConfigureArgs, InterfaceAddress, PortsArgs, PortsResults,
    RestrictArgs, SocketArgs, SocketType,
};
use radix_cap::{Capability, Derivation, ObjectKind, Rights, SlotRef};
use radix_hal::UserMode;
use radix_net::{Ipv4Address, Ipv4Cidr};

use super::memory::cap_rights;
use super::methods::{Answer, answer};
use super::{Result, UserRef, cap_error, error};
use crate::Arch;
use crate::caps;
use crate::fs::inet;
use crate::net::{self, ConfigureError};

/// Calls the method `label` on `cap`, a network capability in `user`'s
/// slot `slot`, kept out of line, as [`super::methods::call`] says.
#[inline(never)]
pub fn method(
    user: &UserRef,
    slot: u32,
    cap: &Capability,
    label: u32,
    args: &[u64; 8],
) -> Result<Answer> {
    let (base, len) = cap.range().ok_or(error(Kernel::WrongKind))?;
    match label {
        abi::SOCKET => {
            let a = SocketArgs::decode(args)?;
            cap.check(ObjectKind::Network, Rights::NONE).map_err(cap_error)?;
            if !cap.rights().contains(Rights::SEND) && !cap.rights().contains(Rights::RECEIVE) {
                return Err(Kernel::MissingRights.status(u32::from(Rights::SEND.bits())));
            }
            if a.family != AddressFamily::Ipv4 {
                return Err(error(Kernel::NotSupported));
            }
            let kind = match a.kind {
                SocketType::Stream => inet::Kind::Stream,
                SocketType::Datagram => inet::Kind::Datagram,
            };
            inet::make(user, a.to, kind, a.flags)?;
            answer(label, 0, [0; 8])
        }
        abi::RESTRICT => {
            let a = RestrictArgs::decode(args)?;
            cap.check(ObjectKind::Network, Rights::COPY).map_err(cap_error)?;
            let offset = u64::from(a.first).checked_sub(base).ok_or(error(Kernel::OutOfRange))?;
            let how = Derivation {
                rights: cap_rights(a.rights),
                badge: None,
                range: Some((offset, u64::from(a.count))),
            };
            let (from, to) = (SlotRef::new(user.cspace, slot), SlotRef::new(user.cspace, a.to));
            caps::with(|store| store.derive(from, to, how))
                .and_then(|done| done)
                .map_err(cap_error)?;
            answer(label, 0, [0; 8])
        }
        abi::PORTS => {
            PortsArgs::decode(args)?;
            cap.check(ObjectKind::Network, Rights::NONE).map_err(cap_error)?;
            // A range of ports is at most the 65,536 of the network.
            let results = PortsResults { first: base as u32, count: len as u32 };
            answer(label, PortsResults::WORDS, results.encode())
        }
        abi::CONFIGURE => {
            let a = ConfigureArgs::decode(args)?;
            cap.check(ObjectKind::Network, Rights::CONTROL).map_err(cap_error)?;
            let (addresses, count) = addresses_in(a.addresses.address, a.addresses.len)?;
            let router = router_in(a.router.address, a.router.len)?;
            net::configure(a.interface, &addresses[..count], router).map_err(|err| match err {
                ConfigureError::NoInterface => File::NoDevice.status(0),
                ConfigureError::Loopback => error(Kernel::NotSupported),
            })?;
            answer(label, 0, [0; 8])
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}

/// The most addresses `configure` takes.
const ADDRESSES_MAX: usize = abi::ADDRESSES_MAX as usize;

/// The `len` bytes at the caller's `address`, read as `configure`'s
/// addresses: InterfaceAddress structures, each a unicast address and a
/// prefix of at most 32, with its reserved bytes zero.
fn addresses_in(address: u64, len: u64) -> Result<([Ipv4Cidr; ADDRESSES_MAX], usize)> {
    let size = InterfaceAddress::SIZE as u64;
    if !len.is_multiple_of(size) || len / size > abi::ADDRESSES_MAX {
        return Err(error(Kernel::InvalidArgument));
    }
    let count = (len / size) as usize;
    let mut bytes = [0u8; InterfaceAddress::SIZE * ADDRESSES_MAX];
    let bytes = &mut bytes[..InterfaceAddress::SIZE * count];
    if !radix_ipc::range::user_bytes(address, len) || !Arch::copy_from_user(bytes, address) {
        return Err(error(Kernel::BadAddress));
    }
    let mut addresses = [Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0); ADDRESSES_MAX];
    for (at, entry) in bytes.as_chunks::<{ InterfaceAddress::SIZE }>().0.iter().enumerate() {
        let ip = unicast(&entry[..4])?;
        let prefix = entry[4];
        if prefix > 32 || entry[5..].iter().any(|&byte| byte != 0) {
            return Err(error(Kernel::InvalidArgument));
        }
        addresses[at] = Ipv4Cidr::new(ip, prefix);
    }
    Ok((addresses, count))
}

/// The `len` bytes at the caller's `address`, read as `configure`'s
/// router: none for 0 bytes, else a unicast address's 4.
fn router_in(address: u64, len: u64) -> Result<Option<Ipv4Address>> {
    match len {
        0 => Ok(None),
        4 => {
            let mut bytes = [0u8; 4];
            if !radix_ipc::range::user_bytes(address, len)
                || !Arch::copy_from_user(&mut bytes, address)
            {
                return Err(error(Kernel::BadAddress));
            }
            unicast(&bytes).map(Some)
        }
        _ => Err(error(Kernel::InvalidArgument)),
    }
}

/// `bytes`, an IPv4 address in network byte order, if it is one a host
/// may have: not 0.0.0.0, a broadcast, multicast or loopback address.
fn unicast(bytes: &[u8]) -> Result<Ipv4Address> {
    let ip = Ipv4Address::new(bytes[0], bytes[1], bytes[2], bytes[3]);
    if ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.is_loopback() {
        return Err(error(Kernel::InvalidArgument));
    }
    Ok(ip)
}
