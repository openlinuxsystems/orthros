<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Random numbers

This note describes the Radix kernel's random number generator as of
Phase 2: where its seed comes from, how it makes output, and how it is
tested. The decision is ADR 0022; the user-facing description is the
*RANDOM NUMBERS* section of `orthros-boot(7)`.

## Crates

```text
kernel/lib/random       radix-random: ChaCha20, BLAKE2s, the generator, the
                        entropy pool, health tests; host and property tests
kernel/arch/x86_64      random.rs: RDSEED, RDRAND and timing jitter; seeds
                        the generator before KASLR and prints the boot line
```

`radix-random` is architecture-independent, `#![forbid(unsafe_code)]`,
and builds for the host, where its tests run. The kernel has one
generator, a static behind a `radix-sync` spin lock, used through
`radix_random::fill`, `u64` and `below`.

## Seeding

The boot trampoline calls `radix_kaslr_choose` at the kernel's link
address, before the image moves, and that function calls
`random::seed` before anything else. Code there may not store pointers or
panic (see `memory.md`), so seeding uses only stack values and statics:

1. a `Pool` (a BLAKE2s-256 hash in progress) takes four 64-bit words from
   `RDSEED`, retried up to 64 times each, and four from `RDRAND`, retried
   up to ten times, each tagged with its source and length;
2. it takes 4096 time-stamp counter readings. Between readings the CPU
   does eight read-modify-writes in a 32 KiB static buffer, at addresses
   derived from the previous reading; `LFENCE` before each `RDTSC` makes
   the reading wait for that work. Every reading is mixed in;
3. `radix_random::seed` finishes the hash and reseeds the generator with
   it: the new key is BLAKE2s of a label, the old key and the seed.

Every source is mixed in whatever its health test says, because adding
input to the hash cannot make the seed easier to guess. The tests only
decide what the boot log reports:

| Source | Fails when |
| --- | --- |
| `RDSEED`, `RDRAND` | the CPU lacks it (reported as missing), a word is not delivered, or a word is 0, all ones or equal to the previous one |
| timing jitter | more than half of the samples are *stuck* (the delta, its difference from the previous delta, or the difference of those is zero), or 32 in a row are |

The boot log then has one line, for example:

```text
Random: ChaCha20 seeded before KASLR from RDSEED, RDRAND and timing jitter (4096 samples, 37 stuck)
Random: ChaCha20 seeded before KASLR from timing jitter (4096 samples, 134 stuck); the CPU has no RDSEED or RDRAND
```

Under QEMU's TCG about 1 to 3 percent of jitter samples are stuck.

## Output

`Generator::fill` runs ChaCha20 (RFC 8439 layout, nonce zero) under the
current key. The first 32 bytes of block 0 become the next key and are
never output; the request gets the other 32 bytes of block 0 and then
blocks 1, 2, and so on. After 4 KiB the generator takes a new key the same
way, so a key makes at most 65 blocks and the fixed nonce never repeats a
(key, block) pair. Keys and blocks are overwritten with zeros when they are
no longer needed. `below(n)` rejects the lowest `2^64 mod n` values, so
every result is equally likely.

KASLR's slide and the places of the direct map and the mapping window are
drawn with `below`.

## Limits

- The spin lock does not mask interrupts, so interrupt handlers must not
  use the generator.
- The generator is seeded once per boot. Phase 5 adds the `getrandom`
  capability, per-CPU generators and reseeding from fresh entropy,
  including interrupt timings.
- The health tests catch broken sources, not predictable ones; nothing
  estimates entropy.

## Verification

Host tests (`./ob test unit`) check ChaCha20 against RFC 8439's vectors,
BLAKE2s against RFC 7693 and Python's `hashlib` around the block size, and
the generator's first two requests after a fixed seed against OpenSSL's
ChaCha20; property tests check that hashing in pieces matches hashing the
whole, that generators seeded alike agree and any change of seed changes
the output, and that successive requests do not repeat. Every boot test
checks the `Random:` line: the sources the seed rests on must include
timing jitter, and, on CPUs that have them, `RDSEED` and `RDRAND`.
