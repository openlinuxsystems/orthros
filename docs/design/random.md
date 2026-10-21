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
`radix_random::fill`, `u64` and `below`. Programs' requests (the random
capability's `read` and `words`, and a ring's `random` operation) go to
a generator per CPU instead (`radix_random::Local`, kept in
`kernel/radix/src/user/random.rs`), so that CPUs asking at once never
wait for each other.

## Per-CPU generators

A `Local` generator is the same ChaCha20 generator with fast key
erasure. On first use, and whenever the kernel's generator has been
seeded again since (a generation number the seeding moves, under the
generator's lock), it takes 32 bytes from the kernel's generator and
mixes them into its key with the reseeding hash. Its output is therefore
as unpredictable as the kernel generator's.

It runs its generator a batch at a time (`radix_random::Batch`, ADR
0042): one request of eight ChaCha20 blocks, whose first 32 bytes become
the next key and whose other 480 it keeps and gives out as programs ask,
zeroing each byte as it goes. This is the buffered form of Bernstein's
fast key erasure, which OpenBSD's `arc4random` and Linux's
`get_random_u64` and `vgetrandom` use too, and forward secrecy holds as
before: the key that made a batch is gone once the batch is made, and
the bytes given out are zero in memory, so a later look at it finds only
bytes still to come, which the key could have made anyway. A seeding of
the kernel's generator throws the rest of the batch away, so nothing
made before it is given out after it. The 64 bytes of `words` then cost
a little over one block instead of two. Requests larger than a batch go
to the generator directly. Only its own CPU uses it, with
preemption off, so it needs no lock (`percpu::Own`, ADR 0040); an
interrupt handler that came in during a request would find it busy and
use the kernel's generator, though none asks. Linux keys its per-CPU
generators from a base generator the same way.

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

Under QEMU's TCG about 1 to 3 percent of jitter samples are stuck. Under
KVM the guest reads the host's own counter, and how steady the work's
timing is depends on the host. On an AMD EPYC 9V45 CI runner about half
the samples were stuck, so the jitter sometimes failed its test while
`RDSEED` and `RDRAND` passed:

```text
Random: ChaCha20 seeded before KASLR from RDSEED and RDRAND; failed its health test: timing jitter (4096 samples, 2079 stuck)
```

That seed is as unpredictable as the CPU's generators. The failed jitter
was mixed in too, and more input to the hash can only add to the seed,
never take from it. The verdict changes what the line claims, not the
seed.

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
- The generator is seeded once per boot. Reseeding from fresh entropy,
  including interrupt timings, is still to come; the per-CPU generators
  already follow a reseed.
- The health tests catch broken sources, not predictable ones; nothing
  estimates entropy.
- The jitter limit of half the samples is our own choice, and it is
  strict. The `jitterentropy` library, whose stuck test this follows,
  takes another sample in place of each stuck one and fails its start-up
  test only when more than 9 in 10 are stuck, next to separate checks for
  a coarse counter. If real machines, not only virtual ones, fail the
  jitter test, the kernel should adopt those rules together.

## Verification

Host tests (`./ob test unit`) check ChaCha20 against RFC 8439's vectors,
BLAKE2s against RFC 7693 and Python's `hashlib` around the block size, and
the generator's first two requests after a fixed seed against OpenSSL's
ChaCha20; property tests check that hashing in pieces matches hashing the
whole, that generators seeded alike agree and any change of seed changes
the output, and that successive requests do not repeat. Every boot test
checks the `Random:` line. On CPUs that have `RDSEED` and `RDRAND`, both
must pass. Timing jitter must pass under TCG, where the counter always
varies enough. Under KVM it may fail, provided the line says so, because
a test cannot choose its runner's CPU and a failed jitter does not
weaken the seed.
