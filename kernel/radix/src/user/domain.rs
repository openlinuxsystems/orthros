// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The methods of resource domains (Phase 9, radix-resource-domain(2)):
//! what Spark uses to give each service a domain of its own, with a CPU
//! weight, a memory limit and an out-of-memory class (`crate::domains`).

use radix_abi::errors::Kernel;
use radix_abi::resource_domain::{
    self as abi, CreateArgs, CreateProcessArgs, InfoArgs, InfoResults, NAME_MAX, NO_LIMIT,
    OomClass as AbiClass, SetLimitArgs, SetWeightArgs, StopArgs, StopResults,
};
use radix_cap::{Rights, SlotRef};
use radix_hal::UserMode;
use radix_klog::info;

use super::methods::{Answer, answer};
use super::{Result, UserRef, cap_error, error};
use crate::domains::{DomainError, DomainRef, OomClass, Settings};
use crate::{Arch, caps, oom};

fn class_of(class: AbiClass) -> OomClass {
    match class {
        AbiClass::App => OomClass::App,
        AbiClass::Service => OomClass::Service,
        AbiClass::System => OomClass::System,
    }
}

fn abi_class(class: OomClass) -> AbiClass {
    match class {
        OomClass::App => AbiClass::App,
        OomClass::Service => AbiClass::Service,
        OomClass::System => AbiClass::System,
    }
}

/// The domain the capability in `slot` names, if it has `needs` on it.
fn domain(user: &UserRef, slot: u32, needs: Rights) -> Result<DomainRef> {
    caps::domain_at(SlotRef::new(user.cspace, slot), needs).map_err(cap_error)
}

/// A method of the resource domain in `slot`, called by `user`'s thread,
/// kept out of line, as [`super::methods::call`] says.
#[inline(never)]
pub fn method(user: &UserRef, slot: u32, label: u32, args: &[u64; 8]) -> Result<Answer> {
    match label {
        abi::CREATE => {
            let a = CreateArgs::decode(args)?;
            let parent = domain(user, slot, Rights::CONTROL)?;
            if a.name.len > NAME_MAX || a.name.len == 0 {
                return Err(Kernel::InvalidArgument.status(0));
            }
            let len = a.name.len as usize;
            let mut bytes = [0u8; NAME_MAX as usize];
            if !radix_ipc::range::user_bytes(a.name.address, a.name.len)
                || !Arch::copy_from_user(&mut bytes[..len], a.name.address)
            {
                return Err(error(Kernel::BadAddress));
            }
            let name = core::str::from_utf8(&bytes[..len])
                .map_err(|_| Kernel::InvalidArgument.status(0))?;
            let settings = Settings {
                weight: a.weight,
                limit_pages: (a.limit_pages != NO_LIMIT).then_some(a.limit_pages),
                class: class_of(a.oom_class),
                supervisor: None,
            };
            let at = SlotRef::new(user.cspace, a.to);
            let made = caps::create_domain(at, &parent, name, settings).map_err(cap_error)?;
            info!(
                "Domains: `{}` made domain `{}` below `{}` ({} class, weight {})",
                user.process.name(),
                made.name(),
                parent.name(),
                made.class().name(),
                made.weight()
            );
            answer(label, 0, [0; 8])
        }
        abi::SET_WEIGHT => {
            let a = SetWeightArgs::decode(args)?;
            let domain = domain(user, slot, Rights::CONTROL)?;
            domain.set_weight(a.weight).map_err(|err| match err {
                DomainError::InvalidArgument => Kernel::InvalidArgument.status(0),
                _ => error(Kernel::NoMemory),
            })?;
            answer(label, 0, [0; 8])
        }
        abi::SET_LIMIT => {
            let a = SetLimitArgs::decode(args)?;
            let domain = domain(user, slot, Rights::CONTROL)?;
            domain.set_limit_pages((a.limit_pages != NO_LIMIT).then_some(a.limit_pages));
            answer(label, 0, [0; 8])
        }
        abi::INFO => {
            InfoArgs::decode(args)?;
            let domain = domain(user, slot, Rights::READ)?;
            let results = InfoResults {
                oom_class: abi_class(domain.class()),
                weight: domain.weight(),
                threads: domain.threads(),
                oom_stops: domain.oom_stops(),
                usage_pages: domain.usage_pages(),
                peak_pages: domain.peak_pages(),
                limit_pages: domain.limit_pages(),
                refused: domain.refused(),
            };
            answer(label, InfoResults::WORDS, results.encode())
        }
        abi::CREATE_PROCESS => {
            let a = CreateProcessArgs::decode(args)?;
            let domain = domain(user, slot, Rights::CONTROL)?;
            let to = SlotRef::new(user.cspace, a.to);
            super::process::create_in(&user.process, domain, a.slots, to)?;
            answer(label, 0, [0; 8])
        }
        abi::STOP => {
            StopArgs::decode(args)?;
            let domain = domain(user, slot, Rights::CONTROL)?;
            let threads = oom::stop_threads(&domain, false);
            info!(
                "Domains: `{}` stopped domain `{}` ({threads} threads)",
                user.process.name(),
                domain.name()
            );
            answer(label, StopResults::WORDS, StopResults { threads }.encode())
        }
        _ => Err(error(Kernel::NoSuchMethod)),
    }
}
