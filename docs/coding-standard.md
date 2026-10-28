<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Coding standard

This standard applies to every crate in the repository. It implements
section 12.2 of the Radix specification; the lints named here are set in the
workspace manifests, so `./ob lint` and CI enforce them.

## Language and toolchain

- Rust, edition 2024, built with the nightly pinned in `rust-toolchain.toml`.
- Kernel code is `#![no_std]`. Host tools use the standard library.
- Code is formatted with rustfmt (`./ob fmt`, settings in `rustfmt.toml`)
  and passes Clippy with every warning treated as an error.

## Lints

Besides Clippy's default lints, every workspace turns on the same 42 of its
pedantic ones: those that find bugs (`cast_ptr_alignment`,
`missing_panics_doc`, `large_stack_arrays`, `no_mangle_with_rust_abi`,
`case_sensitive_file_extension_comparisons`), needless copies and work
(`needless_pass_by_value`, `assigning_clones`, `implicit_clone`,
`large_types_passed_by_value`, `format_collect`, `unused_self`), and
pointer casts that do not say what they keep (`ptr_as_ptr`,
`borrow_as_ptr`, `ref_as_ptr`), with a few that keep code alike
(`manual_let_else`, `semicolon_if_nothing_returned`). The set is listed
once in each manifest, the four workspaces' and the bench program's, and
a test of xtask's checks that the five agree.

The rest of the pedantic group stays off. Most of it is taste
(`must_use_candidate`, `module_name_repetitions`, `doc_markdown`), and the
casting lints (`cast_possible_truncation`, `cast_possible_wrap`,
`cast_sign_loss`, `cast_precision_loss`) fire on thousands of casts that
are right by construction, such as a page count into `usize` or a
register's low bits; a cast that can lose bits or change sign is checked
in review instead, and one that may meet such a value uses `try_from` or
says in a comment why it cannot. Code with reason to differ from a lint
allows it where it is, with a comment saying why.

## `unsafe` code

Only the trusted computing base may use `unsafe`. Today that is:

- the architecture crate `kernel/arch/x86_64`, for everything that touches
  hardware: port and MSR access, page tables, descriptor tables, assembly;
- `kernel/lib/sync`, whose locks hand out references guarded by atomics;
- the device access library `kernel/lib/hw`, which is
  `#![deny(unsafe_code)]` and allows it only in the volatile accesses to
  device registers and DMA memory, in the `unsafe` constructors that
  the architecture calls, whose contracts say what it promises, and in
  the processor's CRC-32C instruction and BMI2's shifts, which read and
  write registers alone;
- the memory manager `kernel/mm`, which is `#![deny(unsafe_code)]` and
  allows it only in the modules that touch the memory it manages (its raw
  accesses, the slab free-object pattern, the heap's `free`, its
  `Platform` trait and the stress test); its allocators' logic is safe
  code, checked on the host and by Kani;
- the kernel crate `kernel/radix`, which is `#![deny(unsafe_code)]` and
  allows it only at marked sites: exporting its entry point, putting the
  locks into panic mode, the context switch and borrowing the running
  thread, per-CPU data, building an open file in place, and handing half
  of a `join` on the caller's stack to another CPU's helper (ADR 0133).

Every other kernel crate, including the other libraries under
`kernel/lib/`, the HAL, the AArch64 stubs and the system call interface
`kernel/abi`, starts with `#![forbid(unsafe_code)]`, and the build tools
under `tools/` inherit `unsafe_code = "forbid"` from their workspace. In
particular every parser of data from outside the kernel
(boot information, ACPI tables, microcode, fonts, the symbol table, the
crash log) is safe code, and so are the capability core `kernel/cap`, which
decides who may use what, the scheduling policy `kernel/sched`, and the
lock checker `kernel/lib/lockdep`.

In user space, `unsafe` is for calls into the kernel and the C library:
`libradix`'s system call stubs, the runtime `radix-rt` (its start-up and
heap), the user-mode tests `usertests`, which make raw calls on purpose,
and the programs on Rust's standard library (`nukesh`, `systools`,
`consoled` and `std-test`) for their libc calls; so does the bench program
`tools/linux-bench`, which measures Linux through libc. The services
`spark` and `storage`, and `hello`, forbid it.

Wherever `unsafe` is used:

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
  algorithms live in crates under `kernel/lib/` that compile for the host
  and carry unit tests (`./ob test unit` runs them).
- Hardware-facing code is tested by booting it in QEMU through `./ob test`.
  A boot test can check the serial log, type `kdb` commands and read the
  framebuffer; paths that only run on failure get a `selftest=` case.
- Kernel code reaches hardware only through the `radix-hal` traits, so it
  also compiles against the AArch64 stubs, which `./ob lint` checks.

## Dependencies

- Third-party crates must come from crates.io under a license `deny.toml`
  allows (Apache-2.0, MIT or compatible), and pass cargo-deny's advisory
  check.
- A new third-party dependency in a kernel crate needs an architecture
  decision record, because it becomes part of the trusted computing base.
