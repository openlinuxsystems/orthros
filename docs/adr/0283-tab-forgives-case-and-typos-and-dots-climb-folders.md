<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0283. Tab forgives case and typos, and `...` climbs folders

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for a Tab that is case and typo tolerant, and `...` for `../..`.

## Decision

- When nothing starts with the word, Tab offers file and command names that start with it ignoring case; failing that, those whose start is one typo away (two for seven letters or more; none under three letters). Only the best tier is offered.
- In a path being completed, `...` is `../..`, `....` is `../../..`; `cd ...` then Tab gives `cd ../../`.
- `compgen` and programmable completions are exact, as in bash.

## Consequences

- A mistyped Tab finds what was meant instead of nothing.
