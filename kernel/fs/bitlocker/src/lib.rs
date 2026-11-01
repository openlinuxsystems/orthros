// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! BitLocker volumes unlocked (ADR 0240): Windows' disk encryption, as
//! Windows 7 and later and BitLocker To Go make it, read from the public
//! description of its format (libbde's, by Joachim Metz).
//!
//! A BitLocker volume begins with a sector of its own in place of its
//! filesystem's boot sector, which says where its three copies of its
//! metadata are ([`format::volume`]). The metadata ([`format::Metadata`])
//! holds the volume's key, the FVEK, wrapped with a volume master key, the
//! VMK, which is in turn wrapped once for each way of unlocking it, its
//! protectors: a password, a recovery key of 48 digits, a key on a USB
//! stick, the TPM, or none while BitLocker is suspended.
//!
//! - [`keys`] (for unlock(8)) turns a password or recovery key into the
//!   key of its protector, as Windows does, and unwraps the VMK and then
//!   the FVEK with AES-CCM.
//! - [`cipher`] decrypts and encrypts the volume's sectors with the FVEK:
//!   AES-XTS, which Windows 10 and later use, and AES-CBC, with or
//!   without the Elephant diffuser of Windows 7, of 128 or 256 bits.
//! - [`format::Layout`] says where a sector of the volume as its
//!   filesystem sees it is: its first sectors are kept, encrypted,
//!   elsewhere on the volume, and the metadata reads as zeros.
//! - [`device::Unlocked`] (for the kernel) is the volume unlocked, a block
//!   device of its own that its filesystem mounts from.
//! - [`make`] (for tests, on the host) encrypts a volume as Windows does.
//!
//! The kernel decrypts in software (`--cfg aes_force_soft`), as it uses
//! no SSE; elsewhere AES-NI is used where the processor has it.

#![no_std]
#![forbid(unsafe_code)]
#![cfg_attr(any(test, feature = "device"), feature(vec_push_within_capacity))]

#[cfg(any(test, feature = "device"))]
extern crate alloc;
#[cfg(any(test, feature = "make"))]
extern crate std;

pub mod cipher;
#[cfg(any(test, feature = "device"))]
pub mod device;
pub mod format;
#[cfg(any(test, feature = "keys"))]
pub mod keys;
#[cfg(any(test, feature = "make"))]
pub mod make;

#[cfg(test)]
mod tests;

pub use format::Error;
