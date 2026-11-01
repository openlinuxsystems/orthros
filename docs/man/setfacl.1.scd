setfacl(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

setfacl - change files' access lists

# SYNOPSIS

*setfacl* [*-bkndRLP*] {*-m*|*-x*} _entries_ _file_ ...

*setfacl* [*-bkndRLP*] {*-M*|*-X*} _list-file_ _file_ ...

*setfacl* *--set*=_entries_ _file_ ...

*setfacl* *--restore*=_dump_

# DESCRIPTION

setfacl changes each file's access list, as the acl package's setfacl
does, with its options and text (ADR 0256): it gives permissions to
users and groups by name, beside the owner, the group and everyone
else the mode names. *getfacl*(1) shows the lists.

_entries_ are separated by commas, or lines in a file, where *#* starts
a comment. Each is one of

	*u*[*ser*]*:*[_user_]*:*_permissions_++
*g*[*roup*]*:*[_group_]*:*_permissions_++
*m*[*ask*][*:*]*:*_permissions_++
*o*[*ther*][*:*]*:*_permissions_

with *d*[*efault*]*:* before it for the default list. A _user_ or
_group_ is a name or a number; with none, the entry is the owner's or
the group's. _permissions_ are letters of *rwx*, *-* for none, or one
octal digit; *X* is execute for a directory, or for a file someone may
execute already. Entries to remove (*-x*, *-X*) have no permissions:
*setfacl -x u:bob notes*.

The mask limits what the named users and groups and the group get, and
is the mode's group bits while the file has a list. setfacl works it out
anew after each change, all the permissions of what it limits together,
unless *-n* or an entry gives it. A default list that is made starts
from the access list's owner, group and others, as libacl's does.

Only a file's owner changes its lists, or a caller with the bypass
right (an administrator through *elevate*(8)); a list sets the mode's
permission bits with it, and one that says no more than a mode is kept
as the mode alone. A directory's default list is what a file or
directory made in it takes, in place of the umask (*radix-posix*(7)).
A symbolic link has no list: one named is changed through. tmpfs,
Btrfs and ext4 keep lists, as Linux keeps them on the disk; FAT, exFAT
and NTFS keep none, and take only a list that says no more than a mode.

# OPTIONS

*-m* _entries_, *--modify*=_entries_
	Set these entries, adding those not there.

*-M* _list-file_, *--modify-file*=_list-file_
	Set the entries in _list-file_, *-* for standard input.

*-x* _entries_, *--remove*=_entries_
	Remove these entries.

*-X* _list-file_, *--remove-file*=_list-file_
	Remove the entries in _list-file_.

*--set*=_entries_, *--set-file*=_list-file_
	Replace the list with these entries, which must name the owner,
	the group and everyone else; default entries replace the default
	list.

*-b*, *--remove-all*
	Remove every entry but the owner's, the group's and everyone
	else's, and the default list.

*-k*, *--remove-default*
	Remove the default list.

*-d*, *--default*
	Make every entry the default list's.

*-n*, *--no-mask*
	Do not work the mask out anew; a list that needs a mask and has
	none takes the group's permissions for it.

*--mask*
	Work the mask out anew, as without *-n*.

*-R*, *--recursive*
	Change what is in the directories too. Default entries change the
	directories alone there.

*-L*, *--logical*
	With *-R*, follow every symbolic link to a directory.

*-P*, *--physical*
	With *-R*, follow no symbolic link, and leave links found alone.

*--test*
	Show each file's new lists and change nothing: the entries, short
	(*u:bob:r--*), with a star between the access list's and the
	default list's.

*--restore*=_dump_
	Set the lists, owners, groups and set-ID and sticky bits of the
	files in _dump_, the text of *getfacl -R*, *-* for standard input.

# EXIT STATUS

0 if every file was changed; 1 if one could not be; 2 for a mistake in
the command line, such as an entry that does not read, as the acl
package's setfacl has it:

	setfacl: Option -m: Invalid argument near character 3

# EXAMPLES

Let bob read _notes_ and pass through the home it is in, without
reading the home:

	setfacl -m u:bob:r notes; setfacl -m u:bob:x ~

Let bob change whatever is made in _shared_ from now on:

	setfacl -d -m u:bob:rw shared

# SEE ALSO

*getfacl*(1), *chmod*(1), *systools*(1), *radix-directory*(2),
*radix-posix*(7)
