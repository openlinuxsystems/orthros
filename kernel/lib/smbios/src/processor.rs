// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Processors (type 4): each socket, and the processor in it as the
//! firmware names it. DSP0134's 7.5.

use crate::{Str, Structure};

/// A processor socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Processor<'a> {
    /// The socket's name, `CPU 0` or `FP8`.
    pub socket: Option<Str<'a>>,
    /// Who made the processor.
    pub manufacturer: Option<Str<'a>>,
    /// The processor's name, as its brand string has it.
    pub version: Option<Str<'a>>,
    /// Whether a processor is in the socket.
    pub populated: bool,
    /// The most it runs at, in MHz.
    pub max_speed: Option<u16>,
    /// Its speed at boot, in MHz.
    pub current_speed: Option<u16>,
    /// Its cores.
    pub cores: Option<u16>,
    /// The cores enabled.
    pub enabled: Option<u16>,
    /// Its threads.
    pub threads: Option<u16>,
}

/// A count in the byte at `offset`, or for 0xff in the word at
/// `extended` (SMBIOS 3.0); none for 0.
fn count(s: &Structure<'_>, offset: usize, extended: usize) -> Option<u16> {
    match s.byte(offset)? {
        0 => None,
        0xff => s.word(extended).filter(|n| *n != 0 && *n != 0xffff).or(Some(0xff)),
        n => Some(u16::from(n)),
    }
}

impl<'a> Processor<'a> {
    /// The processor `s` describes, if it is one.
    pub fn parse(s: &Structure<'a>) -> Option<Processor<'a>> {
        if s.kind != crate::kind::PROCESSOR {
            return None;
        }
        let mhz = |offset| s.word(offset).filter(|mhz| *mhz != 0);
        Some(Processor {
            socket: s.string(0x04),
            manufacturer: s.string(0x07),
            version: s.string(0x10),
            populated: s.byte(0x18).is_some_and(|status| status & 0x40 != 0),
            max_speed: mhz(0x14),
            current_speed: mhz(0x16),
            cores: count(s, 0x23, 0x2a),
            enabled: count(s, 0x24, 0x2c),
            threads: count(s, 0x25, 0x2e),
        })
    }
}
