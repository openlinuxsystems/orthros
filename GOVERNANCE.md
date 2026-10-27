<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Governance

Dukovo is a young project with one maintainer. This file says who decides
what today and how that is meant to grow.

## Guiding principle: favor the user

When a decision reaches a fork in the road, Dukovo favors the user. It is a
users-first system, not a platform run for a company's benefit. Freedom is
at the core of its philosophy: the freedom to use, study, change and share
the system, to run any software the user chooses, and to decide everything
about one's own machine. In practice:

- The user owns the machine. Updates and restarts happen only when the user
  chooses, and nothing is forced.
- Warn and confirm, never forbid. Risky choices, such as a weak password or
  sideloading an unsigned app, get a plain warning and a confirmation, and
  then the user decides.
- No telemetry, no ads, no required online accounts, and no data leaves the
  machine unless the user sends it.
- No age or identity verification, ever. Dukovo never asks for, checks,
  estimates or stores anyone's age, birth date or identity documents: not
  when it installs, when an account is made, at sign-in, in the Store or
  anywhere else, and it gives apps no age signal. An account is a name the
  owner chooses ([ADR 0126](docs/adr/0126-no-age-or-identity-verification.md)).
- Secure defaults, all of which the owner can change.
- Legal and fair to others: every license is honored, and every project we
  build on is credited.

## Maintainer

| Name | GitHub | Role |
| --- | --- | --- |
| Chase | [@openlinuxsystems](https://github.com/openlinuxsystems) | Project lead and maintainer |

Code of conduct reports go to chase@openlinuxsystems.com. Security reports
go through GitHub instead, as [SECURITY.md](SECURITY.md) describes.

The maintainer reviews and merges pull requests, decides the roadmap and
when each phase starts, cuts releases, handles security reports
([SECURITY.md](SECURITY.md)) and enforces the
[code of conduct](CODE_OF_CONDUCT.md).

## How decisions are made

- Day-to-day changes are decided in pull request review. A change merges
  when it meets the definition of done in [CONTRIBUTING.md](CONTRIBUTING.md)
  and the maintainer approves it.
- Decisions that shape the system, or change an earlier one, are recorded
  as [architecture decision records](docs/adr/README.md). Anyone may propose
  one in a pull request; the maintainer accepts or rejects it.
- The order of work is set by [docs/roadmap.md](docs/roadmap.md), which the
  maintainer keeps.
- Every decision is weighed against the
  [guiding principle](#guiding-principle-favor-the-user): at a fork in the
  road, the choice that favors the user wins.
- Where discussion does not reach agreement, the maintainer decides and
  records the reasons.

## Contributions

Contributions are made under Apache-2.0 OR MIT and signed off under the
[Developer Certificate of Origin](CONTRIBUTING.md#developer-certificate-of-
origin).
There is no contributor license agreement, and no contributor assigns
copyright to the project.

## Becoming a maintainer

As others contribute regularly, the maintainer may invite them to become
maintainers of the whole project or of one area (for example the kernel,
userspace or documentation), and add them to the table above in a pull
request. Code owners per area and a voting rule for disagreements between
maintainers will be added here when there is more than one maintainer.

## Changes to this document

Changes to this file, the code of conduct or the security policy are made
in pull requests that the maintainer approves.
