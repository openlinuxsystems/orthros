// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Charging the kernel memory that programs' objects take to resource
//! domains (`crate::domains`).
//!
//! Each process has an account in its domain. A kernel object a program
//! makes through its process (`process.create`) charges the account with
//! the bytes the kernel uses for it, and gives them back when it goes,
//! wherever its capabilities have travelled by then. Domains count pages,
//! so an account charges its domain a page each time its bytes reach into
//! a new one: the domain's usage is the accounts' bytes, rounded up, and a
//! domain at its limit refuses new objects, as it refuses new threads.

use alloc::sync::Arc;

use radix_abi::errors::Kernel;
use radix_mm::{Flags, HEAP, Kmalloc, PAGE_SIZE};
use radix_sync::SpinLock;

use super::{Result, error};
use crate::domains::DomainRef;

/// An account, shared by a process and the objects charged to it.
pub type AccountRef = Arc<Account, Kmalloc>;

/// Kernel memory charged to a domain on behalf of one process.
pub struct Account {
    domain: DomainRef,
    /// The bytes charged.
    bytes: SpinLock<u64>,
}

/// Bytes charged to an account, given back when it is dropped: kept by
/// the object they were charged for.
pub struct Charge {
    account: AccountRef,
    bytes: u64,
}

fn pages(bytes: u64) -> u64 {
    bytes.div_ceil(PAGE_SIZE as u64)
}

impl Account {
    /// A new account in `domain`, with nothing charged.
    pub fn new(domain: DomainRef) -> Option<AccountRef> {
        let account = Account { domain, bytes: SpinLock::new(0) };
        Arc::try_new_in(account, HEAP.with(Flags::MAY_SLEEP)).ok()
    }

    /// The domain it charges.
    pub fn domain(&self) -> &DomainRef {
        &self.domain
    }

    /// The bytes charged now.
    pub fn bytes(&self) -> u64 {
        *self.bytes.lock()
    }

    /// Charges `bytes`, unless the domain or one above it would pass its
    /// limit.
    pub fn charge(account: &AccountRef, bytes: u64) -> Result<Charge> {
        let mut charged = account.bytes.lock();
        let after = charged.checked_add(bytes).ok_or(error(Kernel::NoMemory))?;
        let more = pages(after) - pages(*charged);
        if more > 0 {
            account.domain.charge(more).map_err(|_| error(Kernel::NoMemory))?;
        }
        *charged = after;
        Ok(Charge { account: account.clone(), bytes })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let mut charged = self.account.bytes.lock();
        let after = *charged - self.bytes;
        let fewer = pages(*charged) - pages(after);
        *charged = after;
        if fewer > 0 {
            self.account.domain.uncharge(fewer);
        }
    }
}
