// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A message's info word: label, data word count, capability count and
//! flags (radix-endpoint(2)).

use radix_abi::endpoint::{
    INFO_CAPS_MASK, INFO_CAPS_SHIFT, INFO_LABEL_MASK, INFO_WORDS_MASK, INFO_WORDS_SHIFT,
    MESSAGE_CAPS, MESSAGE_WORDS, MessageFlags, RESERVED_LABELS,
};

/// The bits of an info word that mean something.
const KNOWN: u64 = INFO_LABEL_MASK
    | (INFO_WORDS_MASK << INFO_WORDS_SHIFT)
    | (INFO_CAPS_MASK << INFO_CAPS_SHIFT)
    | MessageFlags::ALL.bits();

/// A message's info word, unpacked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// The label: a method, for a kernel object or a server.
    pub label: u32,
    /// How many data words the message carries, at most 8.
    pub words: usize,
    /// How many capabilities it carries, at most 4, as slot numbers in its
    /// first data words.
    pub caps: usize,
    /// Move the capabilities instead of copying them.
    pub move_caps: bool,
    /// Do not wait.
    pub no_block: bool,
}

/// Why an info word was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoError {
    /// More than eight data words.
    TooManyWords,
    /// More than four capabilities.
    TooManyCaps,
    /// More capabilities than data words to name them in.
    CapsBeyondWords,
    /// A bit that means nothing is set.
    UnknownBits,
}

impl Info {
    /// Unpacks and checks a sender's info word.
    pub const fn decode(word: u64) -> Result<Info, InfoError> {
        if word & !KNOWN != 0 {
            return Err(InfoError::UnknownBits);
        }
        let words = ((word >> INFO_WORDS_SHIFT) & INFO_WORDS_MASK) as usize;
        let caps = ((word >> INFO_CAPS_SHIFT) & INFO_CAPS_MASK) as usize;
        if words > MESSAGE_WORDS as usize {
            return Err(InfoError::TooManyWords);
        }
        if caps > MESSAGE_CAPS as usize {
            return Err(InfoError::TooManyCaps);
        }
        if caps > words {
            return Err(InfoError::CapsBeyondWords);
        }
        Ok(Info {
            label: (word & INFO_LABEL_MASK) as u32,
            words,
            caps,
            move_caps: word & MessageFlags::MOVE_CAPS.bits() != 0,
            no_block: word & MessageFlags::NO_BLOCK.bits() != 0,
        })
    }

    /// Packs it again.
    pub const fn encode(&self) -> u64 {
        let mut word = self.label as u64
            | ((self.words as u64 & INFO_WORDS_MASK) << INFO_WORDS_SHIFT)
            | ((self.caps as u64 & INFO_CAPS_MASK) << INFO_CAPS_SHIFT);
        if self.move_caps {
            word |= MessageFlags::MOVE_CAPS.bits();
        }
        if self.no_block {
            word |= MessageFlags::NO_BLOCK.bits();
        }
        word
    }

    /// The info word the receiver gets: the same label and counts, without
    /// the sender's flags.
    pub const fn delivered(&self) -> u64 {
        Info { move_caps: false, no_block: false, ..*self }.encode()
    }

    /// Whether the label is one of the methods every capability answers,
    /// which never reach a receiver.
    pub const fn is_reserved(&self) -> bool {
        self.label as u64 >= RESERVED_LABELS
    }

    /// An info word with a label and a word count, for replies the kernel
    /// makes.
    pub const fn reply(label: u32, words: usize) -> u64 {
        Info { label, words, caps: 0, move_caps: false, no_block: false }.encode()
    }
}
