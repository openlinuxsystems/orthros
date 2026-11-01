<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0269. Screen-reader mode

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for a screen-reader mode. Colours, faded hints, a right prompt and redraws are noise to a reader.

## Decision

- `[accessibility] screen-reader = true` in settings.toml: no colours (as `NO_COLOR`), no highlighting, no faded hints, no right prompt, and the prompt is words: `~/src, failed 1, admin # `.
- Everything else (Tab, Ctrl+R, Alt+E, abbreviations) works the same.

## Consequences

- One switch, in the file every person can change for themselves.
