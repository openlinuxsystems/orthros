// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of the network capability (Phase 12, radix-network(2),
//! ADR 0183): making network sockets, and deriving a capability for fewer
//! ports or rights, as Spark does for each service.
//!
//! What a network capability allows is checked again where it is used:
//! by the socket methods that take its slot (`files`), not by the socket
//! it made, which is a file like any other.

use radix_abi::errors::Kernel;
use radix_abi::network::{
    self as abi, AddressFamily, PortsArgs, PortsResults, RestrictArgs, SocketArgs, SocketType,
};
use radix_cap::{Capability, Derivation, ObjectKind, Rights, SlotRef};

use super::memory::cap_rights;
use super::methods::{Answer, answer};
use super::{Result, UserRef, cap_error, error};
use crate::caps;
use crate::fs::inet;

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
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}
