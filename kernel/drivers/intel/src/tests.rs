// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The drivers against simulated cards: registers that act as the
//! datasheets say, and a wire whose frames the tests put in and take
//! out through the descriptors in (fake) DMA memory.

mod em;
mod igb;
mod igc;
mod ixgbe;

use radix_hw::{Dma, fake_memory, fake_memory_write};
use radix_nic::PAGE;

/// DMA memory from the host's.
pub fn alloc(pages: usize) -> Option<Dma> {
    Some(Dma::fake(pages * PAGE))
}

/// A delay that only counts.
pub fn no_delay(_us: u32) {}

pub fn read_mem(phys: u64, len: usize) -> std::vec::Vec<u8> {
    let mut out = std::vec![0u8; len];
    fake_memory(phys, &mut out);
    out
}

pub fn write_mem(phys: u64, data: &[u8]) {
    fake_memory_write(phys, data);
}

pub fn u64_at(phys: u64) -> u64 {
    u64::from_le_bytes(read_mem(phys, 8).try_into().unwrap())
}

#[test]
fn identifies_intel_copper_cards_only() {
    use crate::{Kind, em::Mac, identify};
    assert_eq!(
        identify(0x8086, 0x100e),
        Some(crate::Model { name: "82540EM", kind: Kind::Em(Mac::I82540) })
    );
    assert_eq!(identify(0x8086, 0x10d3).map(|m| m.name), Some("82574L"));
    assert_eq!(identify(0x8086, 0x125c).map(|m| m.name), Some("I226-V"));
    // Fiber, an unknown device, another vendor.
    assert_eq!(identify(0x8086, 0x1011), None);
    assert_eq!(identify(0x8086, 0xffff), None);
    assert_eq!(identify(0x10ec, 0x100e), None);
}
