<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0255. Each person has a /tmp of their own

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.7. The security design, which Chase accepted on 2026-10-26,
gives each sign-in "a private /tmp", and ADR 0215 made views for it: a
session's `/tmp` "is a directory of its own, attached at `tmp` in place
of the system's, as Linux's pam_namespace makes `/tmp` one per user";
authd was to build each session's view. Until now a session's root was
the whole tree, stamped for the person without the bypass right (ADR
0250), so everyone shared one `/tmp`, mode 1777.

A shared `/tmp` is where users meet: one reads what another left
readable, or makes a file or a link first at a name another program
will use, the race that `fs.protected_symlinks` and `mkstemp` exist to
lessen on Linux. pam_namespace's usual setup ends it with a `/tmp` for
each user; systemd's `PrivateTmp=` gives each service one, which Spark
does already (ADR 0217).

A `/tmp` for each session, rather than each person, would part a
person's own sessions too: tmux and screen keep their sockets in
`/tmp`, so `tmux attach` on a second console, or later over SSH, would
not find the server the first one started. Nothing is gained from it,
as a person's sessions act for the same user and may read each other's
files anyway.

## Decision

- **A person's session has a view for its root** (ADR 0215), which
  authd builds as it starts it: each name of the root at its place,
  directories and files attached and links copied, but `tmp`, which is
  the person's own: `/system/live/authd/tmp/NUMBER`, theirs, mode 1777
  as `/tmp` is. Their sessions share it; no one else's view holds it.
- **Nothing else changes.** Each place is opened through the person's
  stamped root and attached without the bypass right, so the owners and
  modes of the files decide, as they did without the view. The view
  holds no control right, so a session mounts and attaches nothing,
  which it could not do usefully before either.
- **Root's sessions keep the whole tree**, with the system's `/tmp`, as
  pam_namespace leaves root out, and so do elevated commands (ADR
  0219): `/tmp` in `elevate` is the system's, and a person's own is at
  its real path.
- **It lasts until the computer stops**, in `/system/live`, a tmpfs, as
  `/tmp` does. `/system/live/authd/tmp` is root's, mode 0700, so no one
  lists the others'. Removing an account removes its `/tmp` (ADR
  0220). A folder is named by the account's number, never given again.

## Consequences

- One person no longer sees or races another's temporary files.
- A name added to the root after a session started is not in its view
  until the next sign-in, as a mount namespace on Linux does not see a
  new top-level mount unless it is shared.
- `df /` and `statvfs("/")` in a session describe the view's root, as
  ADR 0215 says; `df` of a folder still describes its filesystem.
- The `signin` boot case has bob write to his `/tmp`, which is his,
  mode 1777; alice's `/tmp` does not hold it, the system's does not
  either, and root finds it at its real path.
- **Not yet:** an elevated command in the person's view, so that
  `elevate` sees their `/tmp` as sudo does under pam_namespace; and
  clearing a person's `/tmp` when their last session ends.

## Alternatives considered

- **A `/tmp` for each session**, as the design's words have it. It
  breaks `tmux attach` and `screen -r` across a person's sessions, for
  no gain, as above.
- **`/tmp` cleaned by a timer and protected by the sticky bit alone**,
  as on Linux without pam_namespace. Files stay readable to others by
  their modes, and names can still be taken first.
- **A per-user `TMPDIR`.** Programs that write to `/tmp` by name, as
  many do, would still share it.
