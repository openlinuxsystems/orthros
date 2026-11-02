mkfs(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

mkfs - make a filesystem of a kind on a disk or partition

# SYNOPSIS

*mkfs* *-t* _kind_ [*-V*] [_option_ ...] _device_ [_size_]

# DESCRIPTION

mkfs runs the tool that makes filesystems of _kind_, with the options and
operands after the kind, as util-linux's *mkfs* does:

[[ *Kind*
:[ *Tool*
|  fat, vfat, msdos
:  *mkfs-fat*(8)
|  exfat
:  *mkfs-exfat*(8)
|  swap
:  *mkswap*(8)

For another kind, mkfs runs the program *mkfs-*_kind_, or *mkfs.*_kind_,
if there is one.

```
mkfs -t fat -n EFI /devices/vda1
```

# OPTIONS

*-t* _kind_, *--type* _kind_
	The kind of filesystem to make. There is no default: util-linux's
	is ext2, which Dukovo does not make.

*-V*, *--verbose*
	Say which tool runs.

# EXIT STATUS

The tool's.

# SEE ALSO

*mkfs-fat*(8), *mkfs-exfat*(8), *mkswap*(8), *fsck-fat*(8), *rdisk*(8)
