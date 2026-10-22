<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0001. Record architecture decisions

- Status: Accepted
- Date: 2026-10-20

## Context

The Radix specification leaves many choices open, and the roadmap runs over
many phases and contributors. Without a record, the reasons behind a choice
are lost and the same questions get argued again.

## Decision

Every decision that the specification leaves open, or that changes an
earlier decision, is written down as a short architecture decision record
in `docs/adr/`, numbered in order, using the template in `0000-template.md`.
Records are never deleted or rewritten; a later record supersedes an
earlier one.

## Consequences

Each pull request that settles an open question adds a record, which makes
reviews about the decision rather than the code. The index in
`docs/adr/README.md` is the place to look before reopening a question.
