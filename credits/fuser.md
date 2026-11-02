<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

Section: In every image
| [fuser](https://github.com/cberner/fuser) 0.16.0 by Christopher Berner, patched for Radix, with [page_size](https://github.com/Elzair/page_size_rs) 0.6.0 by Philip Woods, [zerocopy](https://github.com/google/zerocopy) 0.8.63 by Joshua Liebow-Feeser and Jack Wrenn, [smallvec](https://github.com/servo/rust-smallvec) 1.16.4 by the Servo Project Developers, [memchr](https://github.com/BurntSushi/memchr) 2.8.3 by Andrew Gallant and [log](https://github.com/rust-lang/log) 0.4.35 by the Rust project, from crates.io, on the nix the row of crates patched for Radix names | The filesystem side of Linux's FUSE protocol: fuse-test's filesystem in memory, served through `/devices/fuse` ([ADR 0226](docs/adr/0226-filesystems-that-programs-serve-through-fuse.md)) | MIT (fuser); MIT OR Apache-2.0 (page_size, smallvec, log); BSD-2-Clause OR Apache-2.0 OR MIT (zerocopy); Unlicense OR MIT (memchr) |
