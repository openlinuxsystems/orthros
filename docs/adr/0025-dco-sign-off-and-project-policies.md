<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0025. Contributions are signed off under the DCO, with a code of conduct, governance and security policy

- Status: Accepted
- Date: 2026-10-21

## Context

The project is public and will take outside contributions. It needs a
record that each contributor has the right to submit their change, a code
of conduct, a statement of who decides, and a private way to report
vulnerabilities before the first public image (Phase 14). Most commits so
far were written by an AI assistant for the maintainer, and only a person
can certify where a change came from.

## Decision

- Every commit carries a `Signed-off-by:` line certifying the Developer
  Certificate of Origin 1.1 (`DCO` at the top of the repository). There is
  no contributor license agreement.
- `./ob signoff` checks it, and a `DCO sign-off` CI job runs it on the
  commits each pull request adds. A person signs off their own commits
  with their author email address. A commit written by a tool, such as an
  AI assistant or a GitHub App, needs the sign-off of the person who
  submits it, and sign-offs by tool identities do not count, following
  the Linux kernel's rule that AI agents must not add `Signed-off-by`.
  Merge commits are skipped.
- The commits already on `main` when this was adopted are not rewritten or
  checked; the rule applies from this pull request on.
- `CODE_OF_CONDUCT.md` is the Contributor Covenant 2.1 with its
  attribution, under CC-BY-4.0. That license covers the document only; it
  is not a code license, does not enter `deny.toml` or `kernel/deny.toml`,
  and the document is not part of any image or its license manifest
  (ADR 0021).
- `GOVERNANCE.md` names the maintainer and how decisions are made.
- `SECURITY.md` takes reports through GitHub private vulnerability
  reporting, with acknowledgement within 3 working days, triage within 14
  days and a fix with a published advisory within 90 days.

## Consequences

- Contributors add `-s` to `git commit`, and an AI assistant working for
  a person needs that person's sign-off on its commits before CI passes.
- Private vulnerability reporting must stay enabled in the repository's
  security settings for `SECURITY.md` to work.
- `./ob signoff` only checks that a sign-off is there and who it names. It
  cannot prove the certification is true; that rests on the person.
- When the project has more than one maintainer, `GOVERNANCE.md` gains
  code owners and a rule for disagreements.
