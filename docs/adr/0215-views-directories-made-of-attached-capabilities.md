<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0215. Views: directories made of attached capabilities

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.3. The security design (section 6, decision 3) gives each app
a file tree built for it: its bundle, its data, the system's libraries,
a `/tmp` of its own, and the folders it was granted, each at its real
path, so that a ported program finds `/system/resources/zoneinfo` or
`~/documents/report.odt` where it expects. It chose a kernel **view
object**, a directory whose entries are capabilities attached by name,
like bind mounts made of capabilities, over WASI's way, in which the C
library maps path prefixes to the capabilities it holds: that needs no
kernel, but breaks `ls /`, `..` across entries, symbolic links, getcwd,
and every program that does not go through mlibc. The same object gives
each session a private `/tmp` (the design's gaps of 2026-10-26),
services the set their files declare (13b.4), and containers their
root (release 18).

The tree already resolves every path beneath a directory capability
(ADR 0089): a walk keeps the chain of places it passed, `..` steps back
along it, and a mount is crossed onto by a filter of the nodes something
is mounted on. A view needs only to be another place a walk crosses
onto.

## Decision

- **A view** is a small tree of directories the kernel makes, on a mount
  of its own in no tree, with places of the tree attached at some names
  and symbolic links at others, as a private mount namespace of bind
  mounts is on Linux. radix-directory(2) v8 has three new methods.
  - `make_view` (33) makes an empty one acting for this capability's
    name, and puts a capability to its root in a slot with the read,
    write, control, copy and transfer rights, and the bypass right if
    this capability has it, which the view then has too. It needs no
    right: a view gives nothing its maker does not hold.
  - `attach` (34) attaches the directory, file or device node a
    capability holds at a path in the view, making the view's directories
    on the way (root's, mode 0555; a file's place is a file of mode 0444,
    a device's a node of its kind with no device), so a service's view
    has `/devices/null`. It needs the control right on a view's directory
    and the read right on the capability. A taken name fails with exists,
    a path through what is attached already with in_use, a `..` with
    bad_name; a view's own directory, a pipe or a socket with
    not_supported.
  - `attach_link` (35) makes a symbolic link in the view, so a session's
    view has `/bin` and `/usr` as the tree's root has.
- **Walking a view.** A path crosses onto an attached place as onto a
  mount's root, found through the view's mount's filter, so a name with
  nothing attached costs what it did. Beneath the place, mounts made
  there since are crossed onto as anywhere. `..` from an attached
  place's top goes back to the view's directory it is in, along the
  walk's chain; `..` at the view's root stays there, as `/..` is `/`; a
  link's target that starts with `/` starts again at the view's root;
  and `real_path` gives the view's names. So a program whose root is a
  view gets `/tmp` from getcwd and realpath, not where its session's
  `/tmp` is, and reaches nothing past the view's entries.
- **Names.** A view acts for one name, its maker's capability's, and
  takes only capabilities acting for the same name (not_permitted), so
  nothing in it is reached as another name than through the capability
  attached; one made with the bypass right takes only capabilities with
  it. A capability to the view stamped for another name, which only the
  names capability gives, acts for that name throughout, as stamping any
  directory does.
- **Read-only entries.** A capability attached without the write right
  is read-only beneath the view (read_only, EROFS), and so is every place
  reached through it, a capability opened there included; `statvfs`
  says `ST_RDONLY` there. The view's own directories take no files: only
  `attach` and `attach_link` add to them. Making a name that is there
  fails with exists before read_only, here and on any read-only mount,
  in Linux's order, so `mkdir -p` passes a place attached to a view.
- **The control right** on a view is the right to add to it. Nothing is
  mounted or unmounted beneath a view (not_supported), and `open`
  beneath one never hands on the control right, so a view's holder never
  gets from it the control right over a place it attached, to mount
  there, scrub or trim. A copy derived without the control right is a
  view its holder cannot add to, which is what a session or an app gets.
- **`sync` on a view** syncs the filesystem of each place attached, as
  sync(2) means for a program whose root is a view.
- **Limits.** A view holds at most 512 directories, places and links,
  and 64 KiB of links' targets (no_space). Nothing is detached: a view
  goes with the last capability to it, and until then keeps what it
  attached, so a mount whose root a view attached is in use, as under a
  bind mount on Linux.
- **A session's `/tmp`** is a directory of its own, attached at `tmp` in
  place of the system's, as Linux's pam_namespace makes `/tmp` one per
  user. authd builds each sign-in session's view so (13b.7c, ADR 0255,
  with one `/tmp` for each person rather than each session); the `views`
  boot test builds one the same way, from a root stamped for alice. Until
  sign-in every session is root's, and root's console sessions keep the
  system's root and `/tmp`, as pam_namespace's usual setup leaves root
  out: beneath a view nothing is mounted, scrubbed or trimmed, which
  root's console does until elevate gives the admin set (13b.6).

## Consequences

- Nothing that runs today changes: Spark, its services and the consoles
  keep the system's root. The walk's only new work is a flag carried
  with each place for the read-only entries; every benchmark row keeps
  its speed.
- A program whose root is a view sees the view's tmpfs-like root in
  `statvfs("/")` and `df /`; `/processes/mounts` lists the system's
  mounts, which a view does not change.
- The new `views` boot test builds alice's view and checks it from
  inside: its names and nothing past them, `..` across entries and at
  the root, getcwd, realpath, links of the view's and hers, the
  session's `/tmp`, read-only entries, no mount or attach, and a view of
  her own from what she holds; and from outside, what a view refuses.
- **Not yet:** detaching, which the file dialog's "Open once" may need
  (13d.4); a `/processes` that shows a session only its own processes;
  devices attached one by one, for apps (13d.4); views for services
  (13b.4) and sessions (13b.5).
