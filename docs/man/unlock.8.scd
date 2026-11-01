unlock(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

unlock - unlock a BitLocker volume with its password or recovery key

# SYNOPSIS

*unlock* [*-r*] _device_

# DESCRIPTION

unlock unlocks a volume that Windows encrypted with BitLocker, or a USB
stick it encrypted with BitLocker To Go, as Linux's
*cryptsetup open --type bitlk* does (ADR 0240). The _device_ is a disk
or partition in _/devices_, named as there (*sdb*, *nvme0n1p3*) or by
its path.

It asks for the volume's password or its recovery key, the 48 digits
Windows has one save, with or without their dashes, and shows neither as
it is typed. At a terminal it asks up to three times; from a pipe, once.
A volume whose BitLocker is suspended unlocks without asking.

The volume unlocked is a device of its own, _/devices/_*device*-unlocked,
which *volumed*(8) mounts at _/volumes/_*label*, as it mounts a stick
plugged in. It goes when its disk does. The password and the keys it
opens stay in unlock, and are wiped once used: the kernel is given the
volume's key only.

```
# unlock sdb
unlock: sdb is a BitLocker volume: DESKTOP-1 E: 31/10/2026
Password or recovery key for sdb:
unlock: sdb is unlocked as /devices/sdb-unlocked
# ls /volumes
SECRET  sda5
```

Spark at boot and *volumed*(8) say which volumes are locked. A volume
Windows is still encrypting or decrypting is unlocked only to read, so
that Windows goes on where it stopped. A volume only the TPM or a
startup key protects is not unlocked: give it a recovery key in Windows
first (*manage-bde -protectors -add C: -RecoveryPassword*), and that
unlocks it. Windows' own device encryption gives every drive one, and
keeps it in its user's Microsoft account.

Unlocking needs the administrator: run unlock with *elevate*(8).

# OPTIONS

*-r*, *--read-only*
	Unlock it only to read. A device that refuses writes is unlocked
	only to read anyway.

# EXIT STATUS

0 when the volume is unlocked; 1 when it is no BitLocker volume, its
metadata is damaged, it is of Windows Vista, which unlock does not read,
no password or recovery key typed opens it, it is unlocked already, or
for a mistake in the command line.

# SEE ALSO

*volumed*(8), *eject*(1), *blkid*(8), *elevate*(8), *radix-block*(2),
*systools*(1)
