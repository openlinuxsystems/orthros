<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Boot tests, one file each

A new boot test is a file here, `<name>.rs`, named as the test, which
build.rs finds: no list names it, so pull requests that add tests at the
same time touch no line in common. The file holds

- `//!` lines describing the test, which `ob docs` puts in
  dukovo-boot-tests(7) (ob(1) describes the tests made before);
- `pub const CASE: Case`, whose `name` is the file's name;
- its script and anything else only it uses.

It starts with `use super::super::*;` to reach the test harness. The tests
here run after the others, in the order of their names.
