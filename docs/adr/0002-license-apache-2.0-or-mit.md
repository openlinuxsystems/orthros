<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0002. License Dukovo under Apache-2.0 OR MIT

- Status: Accepted
- Date: 2026-10-20

## Context

The project prefers open source tools and wants contributions from the Rust
ecosystem, where most crates are dual licensed under Apache-2.0 and MIT. The
repository started with only an MIT license.

## Decision

All Dukovo code is dual licensed as `Apache-2.0 OR MIT`, at the user's
option. The repository carries `LICENSE-APACHE` and `LICENSE-MIT`, every
crate sets `license = "Apache-2.0 OR MIT"`, and contributions are accepted
under the same terms (see `CONTRIBUTING.md`).

## Consequences

- Crates can move between Dukovo and the wider Rust ecosystem without
  relicensing.
- Apache-2.0 adds an explicit patent grant; MIT keeps compatibility with
  GPLv2 projects.
- Third-party code must be compatible with both; `deny.toml` enforces the
  allowed licenses in CI. Ported C software keeps its own license and ships
  it with the port.
