getfacl(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

getfacl - show files' access lists

# SYNOPSIS

*getfacl* [*-aceEsRLPnp*] _file_ ...

# DESCRIPTION

getfacl shows each file's access list, as the acl package's getfacl
does and in the same text, which *setfacl --restore* reads back (ADR
0256). An access list gives permissions to the owner, to other users by
name, to the group, to other groups by name, and to everyone else; a
mask limits what the named users, the group and the named groups get.
A directory may have a default list too, which what is made in it
takes. A file with no list of its own shows the one its mode says.

For each file getfacl writes a header, then the list, then the default
list, each entry prefixed with *default:*, then an empty line:

```
# file: users/alice/shared
# owner: alice
# group: alice
user::rwx
user:bob:rwx	#effective:r-x
group::r-x
mask::r-x
other::r-x
default:user::rwx
default:user:bob:rw-
default:group::r-x
default:mask::rwx
default:other::r-x
```

Where the mask takes permissions away from an entry, a tab and
*#effective:* follow it with what the entry gets. The header has a
*flags* line when the file has the set-user-ID, set-group-ID or sticky
bit: *# flags: -s-* for set-group-ID alone. A leading _/_ is left out of
the names, as getfacl says once on standard error, unless *-p*. A _file_
of *-* reads the names from standard input, one a line.

tmpfs, Btrfs and ext4 keep access lists, in the attributes
*system.posix_acl_access* and *system.posix_acl_default*, as Linux
keeps them on the disk. FAT, exFAT and NTFS keep none, and show what
the mode says.

# OPTIONS

*-a*, *--access*
	Show the access list alone.

*-d*, *--default*
	Show the default list alone.

*-c*, *--omit-header*
	Leave out the header.

*-e*, *--all-effective*
	Show the effective permissions of every entry the mask limits.

*-E*, *--no-effective*
	Show no effective permissions.

*-s*, *--skip-base*
	Leave out files whose lists say no more than their mode.

*-R*, *--recursive*
	Show what is in the directories too.

*-L*, *--logical*
	With *-R*, follow every symbolic link to a directory.

*-P*, *--physical*
	Follow no symbolic link: show a link named itself, and leave links
	found while recursing out.

*-n*, *--numeric*
	Show users' and groups' numbers, not their names.

*-p*, *--absolute-names*
	Keep the leading _/_ of the names.

# EXIT STATUS

0 if every file was shown; 1 if one could not be read; 2 for a mistake
in the command line, as the acl package's getfacl has it.

# SEE ALSO

*setfacl*(1), *chmod*(1), *systools*(1), *radix-directory*(2),
*radix-posix*(7)
