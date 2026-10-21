// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Notification words.

/// The bits a capability with `badge` sets: its badge, or bit 0 when it
/// has none, so that every signal leaves a trace.
pub const fn bits(badge: u64) -> u64 {
    if badge == 0 { 1 } else { badge }
}

/// The word after signalling `word` through a capability with `badge`.
pub const fn signal(word: u64, badge: u64) -> u64 {
    word | bits(badge)
}
