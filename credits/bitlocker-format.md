<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: Specifications and papers
- BitLocker's format, as libbde's
  ["BitLocker Drive Encryption (BDE) format"](https://github.com/libyal/libbde/blob/main/documentation/BitLocker%20Drive%20Encryption%20(BDE)%20format.asciidoc)
  by Joachim Metz (GFDL-1.3-or-later) describes it, with IEEE 1619 for
  XTS and NIST SP 800-38C for CCM: `radix-bitlocker`
  ([ADR 0240](docs/adr/0240-bitlocker-volumes-unlock-with-a-password-or-recovery-key.md)),
  which takes facts from it, not its text. We use no code of dislocker,
  of libbde or of cryptsetup.
