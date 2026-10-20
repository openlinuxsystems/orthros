# 0013. Parse the boot information and static ACPI tables in our own crates

- Status: Accepted
- Date: 2026-10-20

## Context

The roadmap's component table lists the rust-osdev `multiboot2` and `acpi`
crates for hardware discovery. Phase 1 needs the Multiboot2 boot
information (memory map, command line, modules, framebuffer, RSDP) and the
static ACPI tables (RSDP, RSDT/XSDT, MADT, FADT, HPET, MCFG). It does not
yet need AML: no device in Phase 1 depends on namespace objects, and PCI
Express devices can use MSI without the `_PRT` routing tables.

Both crates are well made, but they read memory through raw pointers and
handler traits that the kernel must implement with `unsafe`, they cover
far more than Phase 1 uses, and their APIs change between releases. Parsers
of firmware-provided data are a classic source of memory-safety bugs, and
the coding standard confines `unsafe` to the architecture crate.

## Decision

`kernel/lib/bootinfo` and `kernel/lib/acpi` are small parsers written for
Radix. They take byte slices that the architecture crate maps for them,
use no `unsafe` (`#![forbid(unsafe_code)]`), check every length and
checksum, and are unit-tested on the build host against hand-built tables;
the boot tests then run them on QEMU's real tables under both firmwares. Only the tables listed above are understood; others
are listed by signature and skipped.

The AML interpreter is a separate decision, deferred to the phase that
first needs it (power-button and shutdown handling, or legacy interrupt
routing). The candidates stay the rust-osdev `aml` crate or uACPI isolated
in a compartment, as the roadmap's risk table suggests.

## Consequences

- The kernel's view of firmware data is small, safe and tested without
  hardware; a malformed table produces an error message, not a fault.
- The project maintains about 1,400 lines of parsing code and tests that
  the external crates would have provided, and must add tables itself as later
  phases need them (for example SRAT for NUMA in Phase 2, DMAR for an
  IOMMU).
- Revisit when AML is needed: adopting the rust-osdev crates then may make
  it simpler to use their table parsing too.
