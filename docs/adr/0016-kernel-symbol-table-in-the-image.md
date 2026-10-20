# 0016. The kernel carries its own symbol table, written in after linking

- Status: Accepted
- Date: 2026-10-20

## Context

Panic reports must name functions (Phase 1 exit criterion: "a readable
backtrace"), on a machine without a serial port where the report is all
there is. The kernel's DWARF debug information is many megabytes and is not
loaded by GRUB in a usable form. A symbol table can be produced only after
linking, but embedding it as a separate module would let kernel and table
get out of step, and relinking with the table included moves addresses.

## Decision

The image reserves a 1 MiB read-only section, `.ksyms`, filled with zeros
at link time. After linking, `ob build` reads the ELF symbol table, keeps
the function symbols of the higher-half kernel, demangles them, encodes
them in a compact sorted format (`RADIXSYM`, version 1, documented in
`radix-debug(7)` and `kernel/lib/ksyms`) and overwrites the section in
place in the ELF file. No address changes, so the table always matches the
code it describes. The kernel looks addresses up with a binary search, in
safe code, and shows `function+offset` in backtraces and `kdb`.

## Consequences

- Backtraces and `kdb`'s `sym` command work on every image `ob` builds,
  with no extra boot module.
- 1 MiB of the image is reserved; the Phase 1 kernel has about 350
  functions, which take about 26 KiB. `ob build` fails if the table outgrows
  the section, and the section is then enlarged.
- A kernel linked by plain `cargo build` has an empty table and prints bare
  addresses, which still match `addr2line` on the ELF file.
- Revisit with KASLR (Phase 2): the lookup must then subtract the slide,
  which the format's base field already allows.
