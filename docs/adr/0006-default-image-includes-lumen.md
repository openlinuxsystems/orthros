<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0006. The default image is the full 4 GiB image with Lumen

- Status: Accepted
- Date: 2026-10-20

## Context

`ob image` can build a console-only image or one that also includes the
Lumen compositor and its test terminal. Dukovo ships a compositor but no
desktop environment.

## Decision

The default image is the full 4 GiB USB image with Lumen included. Setting
`lumen = false` in `dukovo.toml` (from Phase 16) builds a console-only
image.

## Consequences

- The release image demonstrates the whole system, including graphics.
- Builds of the default image need the Lumen dependencies, so the console
  image remains the faster choice while developing lower layers.
