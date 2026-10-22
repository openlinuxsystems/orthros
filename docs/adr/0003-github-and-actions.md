<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0003. Host on GitHub and run CI on GitHub Actions

- Status: Accepted
- Date: 2026-10-20

## Context

The project needs public hosting, code review and CI that can boot images in
QEMU. The roadmap considered GitHub with Actions and Forgejo with Forgejo
Actions; both are free for open source projects.

## Decision

The repository lives at https://github.com/openlinuxsystems/orthros and CI
runs on GitHub Actions (`.github/workflows/ci.yml`). Every CI step is an
`./ob` command, so the pipeline holds no logic that cannot run locally.

## Consequences

- GitHub-hosted Linux runners expose KVM, so QEMU boot tests run with
  acceleration.
- Because CI only calls `./ob`, moving to Forgejo Actions or a self-hosted
  runner later means rewriting one workflow file, not the build.
- Long-running jobs (fuzzing, file system torture) will need a self-hosted
  runner when they arrive.
