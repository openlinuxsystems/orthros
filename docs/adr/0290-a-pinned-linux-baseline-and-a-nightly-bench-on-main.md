<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0290. The Linux baseline is a pinned kernel, and main is benchmarked every night

- Status: Accepted
- Date: 2026-11-01

## Context

The performance gate holds every operation of `ob bench` to Linux in
the same kind of virtual machine (ADRs 0039, 0107). Until now CI booted
the runner's own kernel for that, copied from `/boot`. GitHub's Ubuntu
images move to each new kernel Ubuntu publishes, so the bar moved with
no change of ours: on the AMD EPYC 9V45 runners tmpfs open+close was
0.87 of Linux on 2026-10-25 (372 against 428 ns) and 1.01 to 1.04 a
week later (357 against 344 ns), Radix's own time unchanged, after the
runners went to Linux 7.0.0-1017-azure. Five of the last five red
Benchmarks jobs were on rows that no pull request among them touched,
and the gate came to be ignored.

Main's own numbers were seldom recorded either: CI cancels a branch's
run when the next push to it starts, and on a busy day every merge to
main cancelled the run of the merge before it.

## Decision

- The baseline kernel is one release of Ubuntu's Azure kernel, the
  flavour CI's runners boot, named with the SHA-256 of its two packages
  (the kernel and its modules, Btrfs's among them) in
  `tools/xtask/src/baseline.rs`. `ob bench` fetches them from Ubuntu's
  archive, or a mirror, into `.cache/linux-baseline`, unpacks them
  without installing them and runs `depmod` on the copy. Ubuntu keeps
  superseded kernels in its archive's pool, so the pin stays fetchable.
  `--linux-kernel` and `OB_LINUX_KERNEL` still boot another kernel;
  `ob fixture` still defaults to the host's own.
- The first pin is 7.0.0-1017-azure, the kernel the runners boot now:
  the bar stays where it is, and the gate's reds on it are fixed in
  Radix, not by choosing an older, slower Linux.
- Moving to a newer kernel is a pull request of its own that changes
  only the pin, so that its Benchmarks job shows what the new kernel
  does to every row before anything else depends on it.
- The report names the baseline's release.
- The bench job is a workflow of its own, `.github/workflows/bench.yml`,
  which CI calls on every pull request and merge as before, and which
  also runs every night on main and on demand. A merge does not cancel
  the nightly run.

## Consequences

- A runner image update no longer changes the gate's verdict. What can
  still change it is the runner's CPU (ADR 0077) and the host's load,
  which the fastest-of-runs rule (ADRs 0046, 0107) is for.
- The bench job downloads about 165 MB of packages the first time, kept
  between runs in Actions' cache.
- `ob bench` needs `dpkg-deb` and `depmod` (kmod), which Debian, Ubuntu
  and Mint have, unless it is given a kernel.
- A row that grows slower over several merges shows on main's nightly
  run rather than first on an unrelated pull request.
