# 0024. The TLA+ models are checked with TLC from the pinned TLA+ tools 1.7.4

- Status: Accepted
- Date: 2026-10-21

## Context

The roadmap's Phase 3 asks for a TLA+ model of capability derivation and
revocation, and its exit criterion requires the model checks to pass. TLA+
specifications are checked with TLC, part of the TLA+ tools
(`tla2tools.jar`, MIT license), which run on Java. Apalache, a symbolic
checker, is the other option; it needs type annotations and is better at
large integer domains than at the tree-shaped state the model has. The
decisions left open are which checker and release, how `ob` gets it, and
how long a check may take.

## Decision

- Models are checked with TLC from the TLA+ tools 1.7.4, pinned with its
  SHA-256 in `tools/xtask/src/tla.rs` and in CI, kept in step by a unit
  test. `ob test tla` downloads the release's `tla2tools.jar` from GitHub
  into `.cache/tla/` on first use, refuses it unless its SHA-256 matches,
  and runs TLC on every model listed in `tla.rs`. `ob clean --all` removes
  it.
- TLC needs Java 11 or later, which `ob doctor` notes (only `ob test tla`
  needs it) and the build container installs (`default-jre-headless`). CI
  checks the models in a job of their own on Temurin 21.
- Each model has configurations sized to finish in about two minutes on 4
  CPUs: a larger one for safety properties, using symmetry, and a smaller
  one without symmetry for liveness, because symmetry is unsound for
  liveness in TLC.
- The tools run only on the build host. Nothing from them is part of an
  image, so they are not in the image's license manifest (ADR 0021).

## Consequences

- The model is exhaustive only for its small configurations; the property
  tests and the self-test cover larger sizes of the same operations.
- A TLA+ tools bump is a reviewed change, like a Kani bump.
- Contributors who do not touch the models need neither Java nor the tools.
- Revisit if a model outgrows TLC's explicit-state search, where Apalache
  or TLC's simulation mode would come in.
