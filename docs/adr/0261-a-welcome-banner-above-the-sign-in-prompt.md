<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0261. A welcome banner above the sign-in prompt

- Status: Accepted
- Date: 2026-10-31

## Context

Chase asked for a fun ASCII art version of the logo above the first
prompt a person sees when the system boots, with the tagline and the OS
version, and approved the mock (design/nukesh/welcome-banner-mock) on
2026-10-31: Duke and the name, "A bright future. A human future." and
"Dukovo Server 0.10.4", directly above the sign-in prompt, without the
host's name, the CPU or a welcome sentence. The consoles showed
/system/settings/issue's line, `Dukovo Server 0.10.4 (tty1)`.

## Decision

- consoled(8) draws the banner in issue's place above the sign-in
  prompt; consoles that start root's shell with no sign-in keep issue's
  text. Plain ASCII only, so every console font draws it; 67 columns,
  so it fits 80. Duke is yellow (the 16-colour tan), the name bold
  white, the version dimmed.
- welcome.toml(5), `/system/settings/console/welcome.toml`: `show`
  (`every-boot`, `first-boot`, `never`), `art` (`duke`, `small`,
  `none`, or a file), `tagline`, `version` (with issue's escapes, so
  it is the running system's), `colors`. Wrong keys are logged and keep
  their defaults.
- A screen narrower than 68 columns or shorter than 18 rows gets the
  name alone, or only the two lines of words.

## Consequences

- The sign-in boot cases look for `Dukovo Server VERSION` without
  `(ttyN)`.
- The tagline is not locked yet; it is one line of welcome.toml.
- **Not yet:** the graphical sign-in's version (Phase 14).

## Alternatives considered

- **Shipping the banner as /system/settings/issue.** issue has no
  colours or size, and a screen too small would wrap Duke.
