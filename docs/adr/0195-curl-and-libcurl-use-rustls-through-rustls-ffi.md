<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0195. curl and libcurl use rustls, through rustls-ffi

- Status: Accepted
- Date: 2026-11-01

## Context

The roadmap's 12a is done when "DHCP and `curl https://` work in QEMU".
curl is also the HTTP library of programs to come: git's HTTPS
transport links libcurl, and so do many ports. curl does TLS through
one of several backends: OpenSSL, GnuTLS, wolfSSL, mbedTLS, or rustls
through rustls-ffi, rustls's C interface, which curl's project
maintains a backend for. Dukovo's TLS is rustls on ring, with no TLS
library in C (ADR 0192).

curl is under its own license, "curl" in SPDX's list, MIT's with a
clause that its holders' names not be used to promote derived works.
rustls-ffi is Apache-2.0, ISC or MIT, at the user's choice. Its default
build is on aws-lc-rs; a feature builds it on ring instead.

curl's release archives are signed with Daniel Stenberg's GPG key. The
build container reaches GitHub's release downloads, but neither GitHub's
key endpoint nor keys.openpgp.org.

## Decision

- **rustls-ffi 0.15.7 is a port,** built by Cargo for Radix with only
  its `ring` and `capi` features, into a static `librustls.a`, which
  holds rustls, ring, Rust's standard library and the rest it needs but
  the C library, with `rustls.h` and a `rustls.pc` for pkg-config. Its
  crates go into the license manifest as the other Rust ports' do.
- **curl 8.23.0 is a port,** from its release archive pinned by SHA-256,
  configured with rustls and without OpenSSL, with zlib and zstd for
  compressed transfers, the threaded resolver, which asks the C library
  and so resolved (ADR 0193), IPv6 and websockets, and with
  `/etc/ssl/certs/ca-certificates.crt`, the system's roots (ADR 0192),
  as its CA bundle. The build fails unless curl's configure took
  rustls, zlib, zstd and the threaded resolver, so that a missing
  library is not silently left out. The archive was checked against the
  `curl-8_23_0` tag of curl's git repository, which it matches but for
  the files curl's release script generates, as the signature could not
  be fetched.
- **`curl` and its manual page ship; libcurl is static** and stays in
  the sysroot with its headers and `libcurl.pc`, for ports to link,
  git first, as every program in `/system` is static (ADR 0048). libcurl's flags carry no path of the build machine.
- **curl's license is allowed for ports** (`PORT_EXTRA_ALLOW`), with its
  text in the license manifest; it asks nothing more than MIT's.
- Left out, each for a library not yet ported: HTTP/2 and HTTP/3
  (nghttp2, ngtcp2 and nghttp3), brotli, international domain names
  (libidn2), the public suffix list (libpsl), SSH (libssh2), LDAP and
  RTMP.

## Consequences

- `curl https://` works with the system's roots, and so does every port
  that links libcurl; TLS for C programs is rustls's, as for Rust's.
- HTTP/1.1 only: servers that offer HTTP/2 still speak 1.1, more slowly
  for many requests on one connection. nghttp2 can come with a later
  part of Phase 12 or the desktop.
- Without libpsl, a program that keeps cookies through libcurl cannot
  tell a public suffix such as `co.uk` from a site, and may accept a
  cookie set for all of it; curl itself keeps no cookies unless asked.
- curl's own TLS settings that rustls lacks fail plainly: no TLS 1.0 or
  1.1, no client certificate engines, no `--ciphers` for TLS 1.2's
  suites beyond rustls's.
- An update of curl needs its archive's SHA-256, and again a check
  against its tag until the signature can be checked.
- Tests: the `network-services` boot test fetches `https://www.test/`
  with a test authority's certificate, refuses it without that
  authority, and checks that `curl -V` names rustls-ffi, zlib and zstd.

## Alternatives considered

- **OpenSSL, or curl's mbedTLS or wolfSSL backends:** a TLS library in
  C, which ADR 0192 keeps out, and a second set of roots and settings.
- **A curl of our own in Rust (reqwest, ureq):** fine for our programs,
  which can use rustls directly, but ports need libcurl's C interface,
  and users expect curl's options.
- **curl from git:** the release archive has the generated configure
  script and manual page, which the git tree needs autotools and Perl
  to make.
