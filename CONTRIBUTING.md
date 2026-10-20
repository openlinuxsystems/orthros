# Contributing to OrthrOS

Thank you for helping. This guide covers how to build and test, what a
change needs before it can merge, and where decisions are recorded.

## Build and test

Everything goes through `./ob`, and CI runs exactly the same commands:

```sh
./ob setup          # once, and whenever rust-toolchain.toml changes
./ob lint           # rustfmt and Clippy, warnings are errors (./ob fmt fixes formatting)
./ob test           # unit tests, then the QEMU boot tests (./ob help test)
./ob docs --check   # man pages and handbook, with completeness checks
```

If your host lacks tools, `./ob doctor` names them, or use
`./ob --container <command>`. `ob(1)` documents every command and option.

## Definition of done

A change merges when:

- CI is green: lint, unit tests, boot tests, docs check, cargo-deny and the
  container build.
- New behaviour has tests that `./ob test` runs. Logic that does not touch
  hardware lives in a crate under `kernel/lib/` that compiles for the host
  and is unit-tested there; behaviour that needs a machine gets a boot test
  case in `tools/xtask/src/test.rs`, which can follow the serial console,
  type `kdb` commands and read the screen.
- Every new command, file format, kernel command-line flag, `kdb` command
  or interface has a man page in `docs/man/` (see "Man pages" below), and
  the handbook is updated where users would look. `./ob docs --check`
  enforces this for `ob` commands, `orthros.toml` keys, kernel flags, `kdb`
  commands and self-tests.
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
Copy [`0000-template.md`](docs/adr/0000-template.md), take the next number,
and add it to the index in [`docs/adr/README.md`](docs/adr/README.md). A
decision is never edited away: a later record supersedes it.

## License of contributions

OrthrOS is dual licensed under the Apache License 2.0 and the MIT license.
Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in OrthrOS by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions. New dependencies must use licenses that `deny.toml` allows.
