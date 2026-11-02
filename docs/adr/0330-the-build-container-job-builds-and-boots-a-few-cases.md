<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0330. CI's build container job builds and boots a few cases; the full container run is nightly

- Status: Accepted
- Date: 2026-11-02

## Context

CI's "Build container" job ran the whole of `./ob --container test` on
every merge to main and on every pull request that touched the ports,
the container or CI. On main's run for #166 it took 88 minutes, while
every other job was done by 31: about 42 minutes building every port
from nothing (main never restores the ports cache), the kernel, the
programs and the unit tests, then 45 minutes for all 108 boot tests one
after another, which the host's jobs already run in three shards. Pull
requests that touched the ports waited on it too: 80 minutes for #165,
89 for #163. It grew with every boot test, by the test's whole time
rather than a third of it, and with every port.

The job exists to check that the container's toolchain builds Dukovo.
The one fault it found that the host's jobs did not was in a port's
build (OpenSSH's configure needed Automake's aclocal, which the
container lacks, #163); the code its tests run is the same code the
host's jobs test.

## Decision

- The job builds every port, the kernel and the programs in the
  container, boots seven cases there (`uefi-usb`, `bios-cdrom`, `posix`,
  `storage`, `tls`, `git-https`, `ssh`: both firmwares, the C ports on
  mlibc, storage, ring, and the git, curl and OpenSSH ports), and builds
  the image with `./ob --container image`. When it runs is unchanged.
- A new workflow, `container.yml`, runs the whole of
  `./ob --container test` every night on main and on demand, without the
  ports cache, so every port is built from nothing at least once a day.

## Consequences

- The job should take about 35 minutes on main (the clean port build
  and the image) and less on a pull request that restores main's ports,
  so it no longer sets the length of CI.
- A test that fails only inside the container shows up in the night's
  run, up to a day after the merge that broke it, instead of on that
  merge. Faults in the container's toolchain still show up at once,
  since every port is still built there.
