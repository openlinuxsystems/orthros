<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0217. Each service runs in a view of what its file declares, and gets its secrets as read-only files

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.4b. The security design's section 8: "Every service, ours or a
catalog bundle's, declares the capabilities it needs with the same
permission names apps use, and Spark gives it exactly those and nothing
else." Until now every service got Spark's root, the whole tree with the
bypass right (ADR 0214), so a bug in diskhealth could rewrite
`/system/settings` or read every home. ADR 0215 gave the kernel views,
directories made of attached capabilities, for this and for sessions and
apps; ADR 0216 gave service files a format that can say what a service
needs. The design's gap 8 adds service secrets: database passwords and
API tokens belong neither in settings files nor in environment
variables, where backups, logs and process listings leak them, so a
service file names its secrets and Spark hands each over as a read-only
file at start, as systemd's credentials do.

## Decision

- **A view per service.** As Spark starts a service it builds a view
  (ADR 0215) and gives it as the service's root: each place at its real
  path, so a ported daemon finds `/system/settings/hosts` where it
  expects, and nothing past them. The view is made from a copy of Spark's
  root without the bypass right, so the service acts for root as the
  owners and modes of its files let it, and the copy the service holds
  has no control right, so nothing more is attached or mounted through
  it. It is built again at each start, from the file as it reads then.
- **What every service gets** (`ALWAYS` in the manifest crate, so
  `service check` knows it too): `/system/commands`, `/system/libraries`,
  `/system/resources` and `/system/settings`, `/processes` and `/kernel`,
  read-only; `/devices/null` read-write and `/devices/zero`, `random`
  and `urandom`; `/system/live/notify` read-only, so it may connect to
  notifyd, as notifications are always allowed; its program; a `/tmp`
  of its own, `/system/live/spark/tmp/NAME`; and the links of the root
  and of `/system`, so `/bin/sh` and `#!/usr/bin/env` work. This is the
  design's "always allowed" list for a service.
- **What a file declares,** in `[permissions]`, beside `network.*`:
  - `files.read = [paths]` and `files.read-write = [paths]`: places of
    the tree, each attached at its path. A place it may change is made if
    it is not there yet, as systemd's StateDirectory= is; a place it only
    reads must be there, or the service does not start, saying which.
  - `files.mount = [paths]`: places it mounts filesystems beneath, as
    volumed does `/volumes`. Nothing is mounted through a view, so each
    is also handed over as a descriptor from 3 on, with the control right,
    and `MOUNT_PLACES` names them in order, as systemd's LISTEN_FDS does
    for sockets.
  - `files.shared = [names]`: folders of `/shared`, the permission list's
    `files.shared`, which a catalog service will use.
  - `runs-with-your-access = true`: the permission list's name for "no
    sandbox; the caller's set". For a service the caller is Spark, so it
    gets Spark's root, the whole tree with the bypass right, in place of a
    view, and cannot also name `files.*`. consoled has it until sign-in
    (13b.5), since it starts root's shells.
  - Places are absolute paths without `.`, `..` or `//`, not `/` itself,
    none within another, none within a place every service reads (name
    the whole place to change it), and none that is or holds
    `/system/state/spark`, where Spark keeps its last good copies and the
    secrets.
- **Secrets.** A file's top-level `secrets = [names]` names them. `service
  secret set NAME SECRET` reads the value from the terminal without
  showing it, or from its input, and keeps it in
  `/system/state/spark/secrets/NAME/SECRET`, mode 0600 in a folder of
  mode 0700; `service secret list NAME` lists the names, and `service
  secret remove NAME SECRET` removes one. No command shows a value back.
  Spark attaches the service's folder read-only at its path and names it
  in `SECRETS_DIRECTORY`; a service whose secret is not set does not
  start, and Spark says which and how to set it, in the log and in the
  answer to `service NAME start`. A value is never put in a variable.
- **Converted files keep the whole tree.** A file of before Phase 13 that
  Spark converts (ADR 0216) gains `runs-with-your-access = true`, with a
  comment, since its service held the whole tree; the admin narrows it.
- **`/volumes` is memory.** The kernel names a mount by the path of the
  capability it is made through, which it knows only for the root of a
  mount; through a capability to a plain directory the mount would be
  named `?/NAME`, and `volumed --list`, eject(1) and findmnt(8) would
  not find it. So Spark mounts a tmpfs on `/volumes` at boot, mode 0755,
  as Linux's `/run/media` is, and hands volumed its root: what is
  mounted there is for this boot anyway.
- **Linux's order of errors.** Making a name that is there fails with
  exists before read-only, so a service's `mkdir -p` of its own folder
  passes the view's read-only directories on the way (ADR 0215); and a
  view attaches device nodes, for `/devices/null`.
- **The shipped services:** logd reads `/devices/kmsg` and changes
  `/system/state/log` and `/system/live/log`; notifyd changes
  `/system/live/notify`; diskhealth reads `/devices`; volumed changes
  `/devices` and mounts beneath `/volumes`; consoled runs with Spark's
  access.

## Consequences

- A broken or compromised logd, notifyd, diskhealth or volumed reaches
  only its own places and what every service reads: not the homes, not
  `/shared`, not the other services' state, not Spark's control socket,
  and not the secrets of others; and without the bypass right, not a file
  whose mode closes it to root.
- Building a view costs a few dozen calls at each start, which no
  benchmark row measures.
- The `services` boot test gives a service a secret with `service
  secret`, starts it, and has it check from inside: its secret as a
  read-only file and in no variable, what every service reads, `/bin/sh`
  and `/usr/bin/env`, its own folder and `/tmp`, nothing past them, and a
  file of mode 000 closed to it. The `logging`, `volumes` and `consoles`
  tests run the shipped services in their views.
- **Not yet:** a name of its own for each service (the design's service
  accounts, with bundles in 13d); the disks one by one for volumed and
  diskhealth, rather than all of `/devices`; a `/processes` that shows a
  service only its own processes; secrets sealed by the TPM (1.4); and
  `service status` listing what a service holds.
