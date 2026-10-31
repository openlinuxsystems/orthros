// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The Intel port switch against a simulated xHCI function's
//! configuration space, whose routing registers keep only the bits their
//! masks allow.

use std::sync::{Arc, Mutex};
use std::vec::Vec;

use radix_hw::pci::{Address, CONFIG_SIZE, Function};
use radix_hw::{Mmio, Registers};

use crate::intel::{self, Routing};

struct Config(Mutex<Vec<u8>>);

impl Config {
    fn word(b: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(b[offset..offset + 4].try_into().unwrap())
    }
}

impl Registers for Config {
    fn read(&self, offset: usize, size: usize) -> u64 {
        if (0x10..0x28).contains(&offset) {
            return 0;
        }
        let b = self.0.lock().unwrap();
        (0..size).fold(0, |v, i| v | u64::from(b[offset + i]) << (8 * i))
    }

    fn write(&self, offset: usize, size: usize, value: u64) {
        let mut b = self.0.lock().unwrap();
        let mut value = value as u32;
        // XUSB2PR and USB3_PSSEN take only what their masks allow.
        if offset == 0xd0 || offset == 0xd8 {
            value &= Config::word(&b, offset + 4);
        }
        for i in 0..size {
            b[offset + i] = (value >> (8 * i)) as u8;
        }
    }
}

fn function(vendor: u16, class: [u8; 3], masks: (u32, u32)) -> Function {
    let mut b = vec![0u8; CONFIG_SIZE];
    b[0..2].copy_from_slice(&vendor.to_le_bytes());
    b[2..4].copy_from_slice(&0x8c31u16.to_le_bytes());
    b[0x09..0x0c].copy_from_slice(&class);
    b[0xd4..0xd8].copy_from_slice(&masks.0.to_le_bytes());
    b[0xdc..0xe0].copy_from_slice(&masks.1.to_le_bytes());
    let config = Arc::new(Config(Mutex::new(b)));
    let address = Address { segment: 0, bus: 0, device: 0x14, function: 0 };
    Function::probe(address, Mmio::fake(config, CONFIG_SIZE)).unwrap()
}

#[test]
fn only_an_intel_xhci_beside_an_intel_ehci_is_switched() {
    let xhci = function(intel::INTEL, [0x30, 0x03, 0x0c], (0x3fff, 0x3f));
    let ehci = function(intel::INTEL, [0x20, 0x03, 0x0c], (0, 0));
    let other = function(0x1b36, [0x30, 0x03, 0x0c], (0x3fff, 0x3f));
    assert!(intel::is_intel_ehci(&ehci));
    assert!(!intel::is_intel_ehci(&xhci));
    assert!(intel::switchable(&xhci, true));
    assert!(!intel::switchable(&xhci, false), "no EHCI: a newer chipset");
    assert!(!intel::switchable(&other, true));
    assert!(!intel::switchable(&ehci, true));
}

#[test]
fn the_ports_the_masks_allow_move_and_come_back() {
    let xhci = function(intel::INTEL, [0x30, 0x03, 0x0c], (0x3fff, 0x3f));
    let before = intel::routing(&xhci);
    assert_eq!(before, Routing { usb2: 0, usb3: 0 });
    let after = intel::switch_ports(&xhci);
    assert_eq!(after, Routing { usb2: 0x3fff, usb3: 0x3f });
    intel::restore(&xhci, before);
    assert_eq!(intel::routing(&xhci), before);
}
