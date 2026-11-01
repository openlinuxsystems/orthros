<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0240. BitLocker volumes unlock with a password or recovery key

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 11f, its last part. Windows encrypts fixed drives with BitLocker,
on many new PCs by itself (device encryption), and USB sticks with
BitLocker To Go. A Dukovo beside Windows, or one a stick is plugged into,
should read and write them, as Linux does with cryptsetup's `bitlk`
type, dislocker and libbde.

- **The format** is not published by Microsoft. It is described by
  libbde's "BitLocker Drive Encryption (BDE) format" by Joachim Metz
  (GFDL-1.3); Radix takes facts from it, not text or code.
- A volume's first sector is BitLocker's (`-FVE-FS-`, or `MSWIN4.1`
  for To Go, whose FAT is a discovery volume for Windows that cannot
  unlock it) and says where three copies of its metadata are. The
  metadata holds the volume's key, the FVEK, wrapped with a volume master
  key, the VMK, which is wrapped again once for each protector: a
  password, a recovery key of 48 digits, the TPM, a startup key, or a
  key in the clear while BitLocker is suspended.
- The FVEK encrypts each sector with AES-XTS (Windows 10 and later) or
  AES-CBC, with Windows 7's Elephant diffuser or without, of 128 or 256
  bits. The volume's first sectors are kept, encrypted, elsewhere; the
  metadata reads as zeros through the unlocked volume.
- The kernel uses no SSE, so AES-NI is not open to it yet.

## Decision

- **radix-bitlocker** (`kernel/fs/bitlocker`, safe Rust) reads the
  first sector and metadata (the first copy that reads), decrypts and encrypts sectors in every method
  above, and is the unlocked volume as a block device:
  - with the RustCrypto crates `aes`, `ccm` and `sha2` (MIT OR
    Apache-2.0), AES in software in the kernel (`--cfg
    aes_force_soft`), and AES-NI where userspace has it;
  - a volume Windows is encrypting or decrypting, part of it encrypted,
    is only read, so that Windows goes on from where it stopped;
  - Windows Vista's volumes, of an older format, are not unlocked
    (`not_supported`).
- **The kernel never sees a password.** *unlock*(8) reads it, or the
  recovery key, unwraps the VMK and then the FVEK, as Windows does
  (SHA-256, stretched 2^20 times, then AES-CCM), and hands only the FVEK
  and its method to the kernel with radix-block(2)'s new `UNLOCK`
  request on the locked device. Every copy of a key is wiped when
  dropped (`zeroize`).
- **The unlocked volume is a device of its own**, `<name>-unlocked`
  (`sdb-unlocked`), which goes with the disk. *volumed*(8) mounts it as
  it mounts any stick, at its label. `UNLOCK` needs the write right on
  the device, but with its read-only flag, which unlocks it only to
  read; a second `UNLOCK` of a device is refused (`in_use`).
- **Naming a locked volume**: *blkid*(8) and *lsblk*(8) show
  `BitLocker`, as libblkid does; Spark at boot and *volumed*(8) after
  say the volume is locked and how to unlock it, rather than that it
  holds no filesystem.
- **Not unlocked**: a volume only the TPM or a startup key protects.
  *unlock*(8) says so, and that Windows' recovery key unlocks it.
- **Tested** by the crate's host tests against what dislocker
  (GPL-2.0) and libbde's `bdeinfo` (LGPL-3.0), which unlock what
  Windows makes, read: a volume Radix encrypts in each method unlocks
  with its password and recovery key and with nothing else, reads back
  as it was, and dislocker reads what Radix's NTFS wrote through the
  unlocked device. The `bitlocker` boot test unlocks a BitLocker To Go
  stick at the console, mounts it, reads and writes it, and dislocker
  decrypts it on the host. Both tools run on the host only.

## Consequences

- A Windows volume or stick BitLocker encrypted is read and written
  with its password or recovery key, which Windows has its user save,
  or keeps in their Microsoft account for device encryption.
- AES in software is several times slower than with AES-NI; letting
  the kernel use AES-NI is a later change.
- unlock(8) asks in a terminal; nothing graphical asks yet.
