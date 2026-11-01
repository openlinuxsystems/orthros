<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Contributing to Dukovo

Thank you for helping. This guide covers how to build and test, what a
change needs before it can merge, and where decisions are recorded.

## Build and test

Everything goes through `./ob`, and CI runs exactly the same commands:

```sh
./ob setup          # once, and whenever rust-toolchain.toml changes
./ob lint           # rustfmt and Clippy, warnings are errors (./ob fmt fixes formatting)
./ob test           # unit tests, then the QEMU boot tests (./ob help test)
./ob docs --check   # man pages and handbook, with completeness checks
./ob test kani      # Kani proofs, when kernel/mm, kernel/cap, kernel/sched,
                    # kernel/ipc or kernel/fs changes
./ob test tla       # TLA+ models (needs Java), when kernel/cap, kernel/sched,
                    # the kernel's block, wake or switch code, its endpoints,
                    # futexes, poll, timers or file locks, FAT's write-back or
                    # libradix's channels change
./ob bench          # system call and IPC costs, next to this machine's Linux
./ob bench --instructions --gate
                    # instructions per operation against their budgets
```

If your host lacks tools, `./ob doctor` names them, or use
`./ob --container <command>`. `ob(1)` documents every command and option.

## Definition of done

A change merges when:

- CI is green: lint, unit tests, boot tests, docs check, Kani proofs, TLA+
  models, the benchmarks and instruction budgets, the shell corpus,
  cargo-deny, the REUSE check, the container build and the DCO sign-off
  check.
- Every commit is signed off (see "Developer Certificate of Origin" below).
- New behaviour has tests that `./ob test` runs. Logic that does not touch
  hardware lives in a crate under `kernel/lib/` that compiles for the host
  and is unit-tested there; behaviour that needs a machine gets a boot test
  case in `tools/xtask/src/test.rs`, which can follow the serial console,
  type `kdb` commands and read the screen.
- Every new command, file format, kernel command-line flag, `kdb` command
  or interface has a man page in `docs/man/` (see "Man pages" below), and
  the handbook is updated where users would look. `./ob docs --check`
  enforces this for `ob` commands and boot tests, `dukovo.toml` keys,
  kernel flags, `kdb` commands and self-tests.
- Any decision the specification leaves open, or any change to an earlier
  decision, has an architecture decision record in `docs/adr/`.
- It follows the [coding standard](docs/coding-standard.md).

## Pull requests

- Branch from `main`, keep each pull request to one topic, and describe what
  a reader would see change.
- Write commit messages with a short imperative subject line (at most 72
  characters), a blank line, and a body that explains why.
- Changing the pinned toolchain in `rust-toolchain.toml` is its own pull
  request, so any fallout is easy to find.
- Update `docs/roadmap.md` when a phase's scope changes.

## Developer Certificate of Origin

Every commit must be signed off. A `Signed-off-by:` line at the end of the
commit message certifies the [Developer Certificate of Origin](DCO)
(DCO), version 1.1: that you wrote the change or otherwise have the right
to submit it under the project's licenses, and that a record of it,
including your sign-off, is kept public. There is no contributor license
agreement; the sign-off is all Dukovo asks.

```sh
git commit -s                  # adds Signed-off-by: Your Name <you@example.org>
git rebase --signoff main      # signs off every commit on your branch
./ob signoff                   # checks the commits your branch adds
```

The line uses your real name or the name you are known by, and the same
email address as the commit's author. Anonymous sign-offs are not accepted.
CI runs `./ob signoff` on the commits each pull request adds; commits that
were on `main` before the rule began
([ADR 0025](docs/adr/0025-dco-sign-off-and-project-policies.md)) are not
checked or rewritten.

**AI assistants.** Only a person can make the DCO's promises. An AI tool
never signs off a commit in its own name, and `./ob signoff` ignores
sign-offs from tool identities such as `noreply@anthropic.com` or GitHub
Apps. When a tool writes a commit, it stays the commit's author or is
credited with a `Co-Authored-By:` line, and the person who directed it,
reviewed the change and submits it adds their own `Signed-off-by:`. That
person is responsible for the change as if they had written it, including
its licensing.
The maintainer has told the AI assistant working for them to add the
maintainer's own sign-off to the commits it writes for them. That is the
maintainer's certification, given standing for those commits and backed by
reviewing each pull request before it merges; no assistant adds anyone
else's sign-off.

## Man pages

Man pages are written in [scdoc](https://git.sr.ht/~sircmpwn/scdoc) syntax
as `docs/man/<name>.<section>.scd`. The first line is `<name>(<section>)`,
and indentation uses tabs. `./ob docs` compiles them into `out/docs/man/`
and fails on any `mandoc -T lint` warning. Preview a page with:

```sh
./ob docs && man -l out/docs/man/man1/ob.1
```

Sections follow the usual convention: 1 user commands, 2 system calls,
3 library interfaces, 4 devices and drivers, 5 file formats, 7 overviews,
8 administration.

## Design notes

`docs/design/` explains how parts of the kernel work, for people changing
them: start with [`boot.md`](docs/design/boot.md) and
[`interrupts.md`](docs/design/interrupts.md). Update the note in the same
pull request as a change that makes it wrong.

## Architecture decision records

`docs/adr/` holds one short Markdown file per decision, numbered in order.
Copy [`0000-template.md`](docs/adr/0000-template.md) and take the next
number of your phase's block (see below). Do not add a row to the table in
[`docs/adr/README.md`](docs/adr/README.md): `./ob docs` makes the index
from the files, and fails if two records take one number. A decision is
never edited away: a later record supersedes it.

## Phases built in parallel

When more than one phase is built at once, each pull request keeps to its
phase's places, so that two of them do not edit the same lines:

- ADR numbers: each phase takes numbers from its own block.
- Boot tests: a new test is a file of its own in
  `tools/xtask/src/test/cases/`, described by its `//!` lines, which
  `ob docs` makes into dukovo-boot-tests(7); no list names it (see the
  README there). `./ob test --list` names every boot test.
- ridl interface numbers: Phase 11 takes `0x0020` to `0x002f`, Phase 12
  `0x0030` to `0x003f` and Phase 13 `0x0040` to `0x004f`. Object kinds and
  Spark's slots are numbered without gaps, so the pull request that merges
  second takes the next free one.
- The roadmap: tick a phase's items in one pull request at the phase's
  end (or a sub-phase's), not in each pull request, since neighbouring
  lines conflict. Credits: a file each in `credits/` (see its README), not a
  row in CREDITS.md.
- `Cargo.lock`: on a conflict, take main's and let Cargo update it, rather
  than merging its lines.
- Merge main into a pull request only when GitHub says it conflicts, or its
  checks fail because of main: each merge runs CI again.

## Conduct, governance and security

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
[GOVERNANCE.md](GOVERNANCE.md) says who maintains Dukovo and how
decisions are made, including the project's
[guiding principle](GOVERNANCE.md#guiding-principle-favor-the-user): when a
change reaches a fork in the road, choose the option that favors the user.
Report security problems privately as described in
[SECURITY.md](SECURITY.md), never in a public issue or pull request.

## License of contributions

Dukovo is dual licensed under the Apache License 2.0 and the MIT license.
Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in Dukovo by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions. New dependencies must use licenses that `deny.toml` allows;
kernel crates must use the permissive licenses in `kernel/deny.toml`, and
`ob image` refuses to build an image otherwise (see
[ADR 0021](docs/adr/0021-license-manifest-in-every-image.md)). A crate that
embeds third-party files under another license declares it in its
`license` field and lists the notice files under
`[package.metadata.dukovo] notices`, as `kernel/lib/fbcon` does.

## Copyright and license headers

The repository follows the [REUSE specification](https://reuse.software/):
every file says who holds its copyright and under which license it is
available, and CI checks this with `reuse lint`. A new file starts with
these two lines, in the file's comment syntax:

<!-- REUSE-IgnoreStart -->
```rust
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT
```
<!-- REUSE-IgnoreEnd -->

Markdown files use an HTML comment, and man pages put the lines as `;`
comments right after the title line. A file that cannot hold a comment, such
as JSON, a font or a generated lock file, is listed in
[`REUSE.toml`](REUSE.toml) instead. The text of every license a file uses is
in `LICENSES/`; other license texts that images need are in
`image/licenses/` (see [ADR 0026](docs/adr/0026-reuse-and-credits.md)). To
check locally, run `reuse lint` (for example with `pipx run reuse lint`).
The REUSE tool is GPL-3.0-or-later; it only checks the repository and is
never part of an image.

## Third-party code and credits

Dukovo gives credit to every project it uses, beyond what the licenses
require. When you add code, data or files from another project:

- Check that its license is allowed where it goes (see above).
- Keep its original copyright and license headers unchanged. Add an
  `SPDX-FileCopyrightText` line for the original authors, and one for
  Dukovo if you change the file, and an `SPDX-License-Identifier` for its
  license; put its license text in `LICENSES/` if it is not there yet.
- Name the source in the commit message: the project, the exact version or
  commit, and the URL, for example
  `Copied from Spleen 2.1.0, https://github.com/fcambus/spleen`.
- Credit the project, with what we use it for and its license, in a file
  of its own in [credits/](credits/README.md), which `ob docs` puts in
  [CREDITS.md](CREDITS.md)'s section. Do the same for a new crate, build tool or CI action, and
  for a project whose design you follow without using its code; also name
  that project in the design note or ADR that draws on it.
- Send fixes we make to upstream code back upstream.
