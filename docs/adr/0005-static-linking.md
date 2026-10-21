<!--
SPDX-FileCopyrightText: 2026 The OrthrOS contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0005. Statically link the programs in /system/bin

- Status: Accepted
- Date: 2026-10-20

## Context

The base system's commands could be linked dynamically against shared
libraries or statically. The base system is updated atomically as a whole
(roadmap Phase 15), so programs and libraries never need to be replaced one
at a time.

## Decision

Programs in `/system/bin` are linked statically, against mlibc for C ports
and the Rust standard library for Rust programs. mlibc's dynamic loader
remains available for bundles that ship private libraries.

## Consequences

- No dynamic loader is needed to bring up the first userspace, which
  simplifies Phases 6 to 9.
- Each binary is self-contained and starts quickly; the base image is
  somewhat larger.
- A security fix in a library means rebuilding the programs that use it,
  which the atomic update model does anyway.
