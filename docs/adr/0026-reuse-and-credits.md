<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0026. The repository follows REUSE and credits every upstream project

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's reproducible-builds section asks that credit go beyond the
legal minimum: code taken from another project keeps its original headers,
the commit names the source, every file carries an SPDX license and
copyright line checked in CI, and CREDITS.md thanks upstream projects,
including those whose ideas we follow without using their code.
[ADR 0021](0021-license-manifest-in-every-image.md) already checks the
licenses of everything in an image, but said nothing about the files in the
repository, and kept license texts in two places: Dukovo's own at the top
of the repository and the rest in `image/licenses/<id>.txt`.

The [REUSE specification](https://reuse.software/spec-3.3/) 3.3 asks for
license and copyright information in every file, or in `REUSE.toml` for
files that cannot carry it, and for the text of every license used in
`LICENSES/<id>.txt`. Its tool fails on texts in `LICENSES/` that no file
uses, so the texts that only image components need cannot all move there.

## Decision

- Every file starts with `SPDX-FileCopyrightText` and
  `SPDX-License-Identifier` lines in its comment syntax; man pages carry
  them as scdoc comments after the title line. Files without comments
  (Cargo lock files, the JSON target specification, the Spleen fonts) are
  covered by `REUSE.toml`. Copyright is held by "The Dukovo contributors",
  who keep the copyright in what they contribute.
- `LICENSES/` holds the licenses that files in the repository use, now
  Apache-2.0, MIT, BSD-2-Clause (the fonts), CC-BY-4.0 (the code of
  conduct) and LicenseRef-DCO (the verbatim DCO text). `image/licenses/LICENSE-<id>.txt`
  holds the other texts the image needs; REUSE treats files named
  `LICENSE-*` as license files. `ob image` takes a text from `LICENSES/`
  when it is there and from `image/licenses/` otherwise, and a unit test
  checks that no text is in both. `LICENSE-APACHE` and `LICENSE-MIT` stay at
  the top for GitHub and Cargo, and the same test checks that they match
  `LICENSES/`.
- A CI job runs `reuse lint` with the REUSE tool 6.2.0 from PyPI. The tool
  is GPL-3.0-or-later, which is fine for a check that never ships.
- CREDITS.md lists the projects in the repository and the image, the crates,
  the build, test and CI tools, the specifications we implement, and the
  projects whose design we follow. CONTRIBUTING.md states the rule for
  copied code: keep the original headers, name the project, version and URL
  in the commit, and add the project to CREDITS.md.

## Consequences

- A new file without SPDX lines, or a license used without its text, fails
  CI before review.
- Phase 4 and later work must add the header to each new file.
- Each port recipe (Phase 8) and bundle will need the same information;
  the users' view of the credits (an Acknowledgements page, About boxes,
  credits(1)) is generated from the image manifest when those parts exist.
- When the REUSE specification changes, the pinned tool version is bumped
  in its own pull request.
