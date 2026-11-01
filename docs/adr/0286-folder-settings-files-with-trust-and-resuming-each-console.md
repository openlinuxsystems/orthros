<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0286. Folder settings files with trust, and resuming each console where it was

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for direnv-style folder settings with a trust prompt, and for a new console to open where the last one there was.

## Decision

- **`.nukesh.toml`** in a folder or above it: `[env]` variables (exported) and `[abbr]` abbreviations, set while the shell is in that folder or under it, and undone (old values back) when it leaves. It runs nothing; other sections are mistakes.
- **Trust:** a file applies only after `trust`, which keeps a copy of it in `~/.local/state/nukesh/trusted/`; a file that changes is not trusted until trusted again; `trust -r` takes it back. An untrusted file is named once a session. `[folders] project-files = false` turns it off.
- **Resume:** consoled sets `XDG_VTNR` for each console. Each shell on a console notes its folder in `~/.local/state/nukesh/where`; a console's first shell (SHLVL 1) that starts at home goes back there. Terminal windows and ssh have no `XDG_VTNR` and are left alone. Signed-in sessions get a fresh environment from authd, which does not pass it yet. `[folders] resume = false` turns it off.

## Consequences

- Project variables come and go with the folder, and only from files the person read.
