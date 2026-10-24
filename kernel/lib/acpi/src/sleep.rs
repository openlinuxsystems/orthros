// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The sleep types of ACPI's sleep states, which powering off needs
//! (Phase 9).
//!
//! To enter sleep state Sx (S5 is soft off), the kernel writes the state's
//! SLP_TYPa value with SLP_EN into the PM1a control register, and SLP_TYPb
//! into PM1b's, if there is one (ACPI 6.5, section 16.1.6). The values are
//! the first two elements of the `\_Sx` package in the DSDT or an SSDT:
//! AML, which Radix does not interpret yet (ADR 0013). [`sleep_types`]
//! finds the package's encoding instead, as firmware writes it:
//!
//! ```text
//! NameOp ('\')? '_' 'S' '5' '_' PackageOp PkgLength NumElements element...
//! ```
//!
//! where each element is a constant: ZeroOp, OneOp, OnesOp, or a byte,
//! word or double word after its prefix (section 20.2.3).

/// AML's NameOp.
const NAME_OP: u8 = 0x08;
/// AML's root prefix, `\`.
const ROOT: u8 = b'\\';
/// AML's PackageOp.
const PACKAGE_OP: u8 = 0x12;

/// The SLP_TYPa and SLP_TYPb values of sleep state S`state` (0 to 5),
/// from an AML table's bytes (a DSDT's or an SSDT's, header included or
/// not), if it names the state's package.
pub fn sleep_types(aml: &[u8], state: u8) -> Option<(u8, u8)> {
    if state > 5 {
        return None;
    }
    let name = [b'_', b'S', b'0' + state, b'_'];
    let mut from = 0;
    while let Some(found) = aml.get(from..)?.windows(4).position(|w| w == name) {
        let at = from + found;
        from = at + 1;
        let before = aml.get(..at)?;
        let named = matches!(before, [.., NAME_OP] | [.., NAME_OP, ROOT]);
        if named && let Some(types) = package(aml.get(at + 4..)?) {
            return Some(types);
        }
    }
    None
}

/// The first two elements of the package `bytes` start with, each cut to
/// the three bits a sleep type has.
fn package(bytes: &[u8]) -> Option<(u8, u8)> {
    let (&op, rest) = bytes.split_first()?;
    if op != PACKAGE_OP {
        return None;
    }
    // PkgLength: bits 6 and 7 of its first byte count the bytes after it.
    let lead = *rest.first()?;
    let rest = rest.get(1 + usize::from(lead >> 6)..)?;
    let (&elements, rest) = rest.split_first()?;
    if elements < 2 {
        return None;
    }
    let (a, rest) = constant(rest)?;
    let (b, _) = constant(rest)?;
    Some((a & 7, b & 7))
}

/// The low byte of the integer constant `bytes` start with, and the bytes
/// after it.
fn constant(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let (&op, rest) = bytes.split_first()?;
    let size = match op {
        // ZeroOp, OneOp, OnesOp.
        0x00 => return Some((0, rest)),
        0x01 => return Some((1, rest)),
        0xff => return Some((0xff, rest)),
        // BytePrefix, WordPrefix, DWordPrefix.
        0x0a => 1,
        0x0b => 2,
        0x0c => 4,
        _ => return None,
    };
    Some((*rest.first()?, rest.get(size..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_qemu_s5() {
        // QEMU's DSDT: Name (_S5, Package (4) { Zero, Zero, Zero, Zero }),
        // after its S3 and S4 packages.
        let aml = [
            0x10, 0x05, b'\\', 0x08, b'_', b'S', b'3', b'_', 0x12, 0x06, 0x04, 0x01, 0x01, 0x00,
            0x00, 0x08, b'_', b'S', b'5', b'_', 0x12, 0x06, 0x04, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(sleep_types(&aml, 5), Some((0, 0)));
        assert_eq!(sleep_types(&aml, 3), Some((1, 1)));
        assert_eq!(sleep_types(&aml, 4), None);
    }

    #[test]
    fn reads_prefixed_constants_and_root_names() {
        // Name (\_S5, Package (0x04) { 0x07, 0x07, 0, 0 }), as many PCs.
        let aml = [0x08, b'\\', b'_', b'S', b'5', b'_', 0x12, 0x0a, 0x04, 0x0a, 0x07, 0x0a, 0x07];
        assert_eq!(sleep_types(&aml, 5), Some((7, 7)));
        // A word prefix and a two-byte PkgLength.
        let aml = [0x08, b'_', b'S', b'5', b'_', 0x12, 0x40, 0x00, 0x02, 0x0b, 0x05, 0x00, 0x01];
        assert_eq!(sleep_types(&aml, 5), Some((5, 1)));
    }

    #[test]
    fn skips_what_is_not_the_package() {
        // A method of that name and a mention in a string come first.
        let aml = [
            0x14, b'_', b'S', b'5', b'_', 0x00, 0x0d, b'_', b'S', b'5', b'_', 0x00, 0x08, b'_',
            b'S', b'5', b'_', 0x12, 0x05, 0x02, 0x0a, 0x03, 0x00,
        ];
        assert_eq!(sleep_types(&aml, 5), Some((3, 0)));
        // Cut short, or with one element: nothing.
        assert_eq!(sleep_types(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x05, 0x02, 0x0a], 5), None);
        assert_eq!(sleep_types(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x03, 0x01, 0x00], 5), None);
        assert_eq!(sleep_types(&[], 5), None);
        assert_eq!(sleep_types(&[0x08, b'_', b'S', b'5', b'_'], 9), None);
    }
}
