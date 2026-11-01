// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The messages of the network services, read and written: DNS's, for
//! resolved(8) and resolvectl(1) (ADR 0193), and NTP's with Network Time
//! Security's, for timed(8) (ADR 0194). The servers of the
//! network-services boot test build their answers with the same code, so
//! it is a library of its own. Nothing here touches the network: each
//! function takes and gives bytes.

pub mod dns;
pub mod ntp;
