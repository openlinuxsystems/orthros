<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Governance

OrthrOS is a young project with one maintainer. This file says who decides
what today and how that is meant to grow.

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
- Where discussion does not reach agreement, the maintainer decides and
  records the reasons.

## Contributions

Contributions are made under Apache-2.0 OR MIT and signed off under the
[Developer Certificate of Origin](CONTRIBUTING.md#developer-certificate-of-origin).
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
