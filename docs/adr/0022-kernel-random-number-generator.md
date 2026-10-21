# 0022. The kernel's random numbers come from ChaCha20 seeded through BLAKE2s from every source

- Status: Accepted
- Date: 2026-10-20

## Context

KASLR (ADR 0018) needs random numbers before the kernel has moved, and
Phase 5 gives every process a `getrandom` capability for its own ASLR,
TLS, SSH keys, password salts and signatures. Until now KASLR read
`RDSEED` or `RDRAND` and mixed the result with the time-stamp counter
through a 64-bit finaliser, which is not a cryptographic generator and
trusts the CPU's generator completely.

The plan (updated 2026-10-20, Phase 2) asks for a kernel CSPRNG: ChaCha20,
seeded from `RDSEED` and `RDRAND` plus timing jitter, never trusting a
single source, ready before KASLR picks its offset.

The CPU's generators cannot be audited, and some have failed in the field
(AMD CPUs whose `RDRAND` returned all ones after a resume). Timing jitter
is available on every machine but its entropy is hard to measure, and in
a virtual machine it depends on the host. No source is trustworthy alone.

## Decision

- A new architecture-independent crate, `radix-random`, holds the
  generator: ChaCha20 (RFC 8439) with fast key erasure, as Linux and the
  BSDs use. Each request takes the first 32 bytes of the key stream as the
  next key and serves the request from the rest, so output already handed
  out cannot be recomputed from a later copy of the state. A request takes
  a new key every 4 KiB.
- Seeds go through a pool that hashes every input with BLAKE2s-256
  (RFC 7693), each tagged with its source and length, and reseeding hashes
  the old key with the new seed. The seed is therefore unpredictable as
  long as any one source is, which is what "never trusting a single
  source" means here: every source is always mixed in.
- On x86-64 the sources are 256 bits of `RDSEED`, 256 bits of `RDRAND`
  and 4096 time-stamp counter readings taken around a small memory
  workload whose addresses depend on the previous timing.
- Health tests decide what the boot log reports, never what is mixed in: a
  hardware word that is zero, all ones or a repeat fails its source; a
  jitter sample whose delta, or its first or second difference, is zero is
  stuck, and the jitter fails if more than half its samples are stuck or 32
  are stuck in a row. The `Random:` boot line names the sources that
  passed, and those that failed or are missing.
- The boot trampoline's call into `radix_kaslr_choose` seeds the generator
  first, at the link address, so the generator is ready for KASLR whether
  or not KASLR is on. KASLR and the layout draw uniform numbers from it
  (`radix_random::below`, rejection sampling instead of a biased modulo).
- ChaCha20 and BLAKE2s are our own code, not a third-party crate (the
  coding standard asks for an ADR per kernel dependency, and both
  algorithms are short). Host tests check them against the RFC vectors,
  OpenSSL and Python's `hashlib`; proptest checks the generator and the
  hash's incremental interface.

## Consequences

- KASLR's offsets are now as unpredictable as the best of three sources,
  not just the CPU's generator.
- Seeding takes about 4096 short timed loops at boot, a few milliseconds.
- There is one generator behind a spin lock that does not mask
  interrupts, so interrupt handlers must not use it. Per-CPU generators, a
  periodic reseed from fresh entropy (including interrupt timings), and
  the `getrandom` capability come with Phase 5, when user programs first
  need random numbers.
- The jitter tests are a sanity check, not an entropy estimate: they catch
  a frozen or coarse counter, not a counter that an attacker can predict.
  If a machine fails them, the boot line says so.
- Revisit if a hardware architecture without a cycle counter arrives, or
  when a measured-entropy estimate (NIST SP 800-90B) is needed.
