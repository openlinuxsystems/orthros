# Coding standard

This standard applies to every crate in the repository. It implements
section 12.2 of the Radix specification; the lints named here are set in the
workspace manifests, so `./ob lint` and CI enforce them.

## Language and toolchain

- Rust, edition 2024, built with the nightly pinned in `rust-toolchain.toml`.
- Kernel code is `#![no_std]`. Host tools use the standard library.
- Code is formatted with rustfmt (`./ob fmt`, settings in `rustfmt.toml`)
  and passes Clippy with every warning treated as an error.

## `unsafe` code

Only the trusted computing base may use `unsafe`. Today that is the kernel
crate `kernel/radix` and the architecture crate `kernel/arch/x86_64`. Every
other crate, including kernel libraries such as `kernel/lib/bootinfo` and
everything under `tools/`, starts with `#![forbid(unsafe_code)]` or inherits
`unsafe_code = "forbid"` from its workspace.

Inside the trusted computing base:

- Every `unsafe` block is preceded by a `// SAFETY:` comment that states
  which invariants make it sound (Clippy `undocumented_unsafe_blocks` is
  `deny`).
- Every `unsafe fn` has a `# Safety` section saying what the caller must
  guarantee (Clippy `missing_safety_doc` is `deny`).
- Unsafe operations inside an `unsafe fn` still need their own `unsafe`
  block (`unsafe_op_in_unsafe_fn` is `deny`).
- Keep `unsafe` blocks small, and wrap them in safe functions whose
  signatures make misuse impossible, as `Serial` does for port I/O.

## Errors and panics

- Recoverable failures return `Result` with an error type that says what
  went wrong. Errors are not silently discarded; where a result really
  cannot fail, say so in a comment next to the discard.
- Production kernel paths do not panic. A panic is acceptable only for an
  invariant violation the kernel cannot continue from, such as being started
  without a Multiboot2 boot loader.
- Resources are released by `Drop` (RAII), not by hand on each exit path.

## Documentation

- Every crate and module starts with a `//!` comment saying what it is for.
- Every public item has a doc comment (`missing_docs` is on).
- Non-obvious subsystems get a design document under `docs/design/` when
  they land, and decisions get an architecture decision record.
- Every command, file format, kernel command-line flag and interface gets a
  man page; see [CONTRIBUTING.md](../CONTRIBUTING.md).

## Testing

- Code that can run on the host does: parsers, data structures, policy and
  algorithms live in crates that compile for the host and carry unit tests.
- Hardware-facing code is tested by booting it in QEMU through `./ob test`.

## Dependencies

- Third-party crates must come from crates.io under a license `deny.toml`
  allows (Apache-2.0, MIT or compatible), and pass cargo-deny's advisory
  check.
- A new third-party dependency in a kernel crate needs an architecture
  decision record, because it becomes part of the trusted computing base.
