<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0256. Access lists and extended attributes, as Linux keeps them

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.7. Since ADR 0214 the kernel checks owners, groups and modes
as Linux does, "without access lists (those come in 13b.7)". A mode
gives one owner, one group and everyone else; sharing a folder with one
other person means a group made for the two of them, which only an
administrator makes. Linux's POSIX access lists let the owner name
other users and groups with permissions of their own, and Ubuntu,
Fedora and every desktop that shares folders use them. ADR 0252 left
"a group that reads the whole log" to them too.

Linux keeps the lists as extended attributes, `system.posix_acl_access`
and `system.posix_acl_default`, beside the user's own `user.` attributes
that file managers, browsers (where a download came from) and backup
tools write, and `trusted.` and `security.` ones. A disk Dukovo shares
with Linux has them already: Btrfs keeps them as `XATTR_ITEM`s and ext4
in the inode and in a block of their own. Chase chose on 2026-10-31 to
finish the lists before the pause after 13b.

## Decision

- **The rules are Linux's.** The kernel's check consults a file's access
  list for anyone but its owner, while the mode's group bits are not
  all zero (`acl_permission_check`): the owner's entry, a named user's,
  the groups' (the best one that grants), then the others', with the
  mask limiting all but the owner's and the others'. The mode's group
  bits are the mask while there is a list; `chmod` sets the list's
  owner, mask and others, and a list set sets the mode
  (`posix_acl_update_mode`). A list the mode says all of is not kept.
- **A directory's default list** is what a file or directory made in it
  takes (`posix_acl_create_masq`), and then the umask has no say: the
  kernel carries the umask into the filesystem layer (`Who::file_mask`)
  so it can leave it out. A directory made there takes the default list
  as its own default too.
- **Extended attributes keep Linux's namespaces, limits and errors**
  (`xattr_permission`): `user.` names on files and directories, read and
  written with read and write permission, in a sticky directory changed
  only by its owner; `trusted.` names, which need the bypass right to see or
  change; `security.` ones, changed with it; the two lists, changed by
  the owner, never on a symbolic link. Names are at most 255 bytes,
  values 64 KiB; ENODATA, ERANGE, E2BIG and EOPNOTSUPP mean what they do
  on Linux. Any other namespace is EOPNOTSUPP.
- **The kernel calls are radix-directory(2)'s `get_attribute`,
  `set_attribute`, `list_attributes` and `remove_attribute` (36-39) and
  radix-file(2)'s (27-30)**, which a directory's capability takes too;
  the file domain gains `no_attribute` (ENODATA). The C library's
  `setxattr`, `getxattr`, `listxattr`, `removexattr` and their `l` and
  `f` forms are mlibc's own, over these.
- **Filesystems keep them as Linux does**, so a disk moves between the
  two with its lists:
  - Btrfs: an `XATTR_ITEM` of the inode, keyed by the name's CRC-32C as
    a directory entry is, the value in the item, in a tree block; a
    value too large for one is ENOSPC.
  - ext4: in the inode after `i_extra_isize`, then in a block of
    attributes (`i_file_acl`), where Linux's `ext4_xattr_set_handle`
    puts them, with entry and block hashes and `metadata_csum`
    checksums. A block shared with another inode is copied before it
    changes. The lists are kept in ext4's own form, version 1. A value
    in an inode of its own (`ea_inode`) is listed and kept, not read or
    changed.
  - tmpfs keeps them in memory, up to 128 KiB a file, charged to the
    filesystem's size. FAT and exFAT keep none: EOPNOTSUPP.
- **The lists are read once and kept** with each node, a directory's
  default list too, and read again after the attribute changes, as
  Linux keeps `i_acl` and `i_default_acl`: a check, or making a file,
  costs nothing more where there is no list.
- **`getfacl` and `setfacl` are systools**, with the acl package's
  options, text, mask rules and messages, so scripts and habits carry
  over.

## Consequences

- People share folders with each other without an administrator, and
  tools that keep attributes (rsync `-AX`, `cp --preserve=all`, `tar
  --xattrs`) keep them on Dukovo's disks, once those ports ask for them.
- Btrfs and ext4 volumes Linux made show their lists and attributes,
  and what Dukovo writes passes `btrfs check` and `e2fsck`, which the
  host tests run.
- A check for someone other than the owner, and making a file, read
  the lists once per node kept in memory.
- Not yet: rich ACLs and NFSv4 ACLs (13f, SMB and NFS), `security.`
  modules (none), and the attributes of a whole volume (Btrfs
  properties), which Linux keeps apart. ADR 0214's "without access
  lists" holds no more.
