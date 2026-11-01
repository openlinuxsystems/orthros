<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0284. Syntax mistakes show before Enter

- Status: Accepted
- Date: 2026-11-01

## Context

Chase asked for syntax mistakes to show red before Enter.

## Decision

- A finished line the parser rejects (not one that is only unfinished) has the token it is near red and underlined, as an unknown command is (ADR 0258): the first of that token's where the line up to it is already wrong. After the cursor, faded, is why: `` ← `fi` cannot go there``.

## Consequences

- `ls | | wc` or a stray `fi` is seen before running.
