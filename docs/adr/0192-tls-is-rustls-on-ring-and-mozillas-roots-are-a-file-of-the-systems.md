<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0192. TLS is rustls on ring, and Mozilla's roots are a file of the system's

- Status: Accepted
- Date: 2026-10-31

## Context

The roadmap's 12a is done when `curl https://` works in QEMU, and
resolved's DNS over TLS and HTTPS (12a.5), timed's Network Time Security
(12a.6), and later admind, certd and sshd's neighbours all need TLS.
Chase chose rustls on ring for Phase 12 (2026-10-30): ring is the crypto
boringtun, WireGuard's, needs too, so one crypto core serves both, where
aws-lc-rs, rustls's default, brings a CMake build of AWS-LC's C. The
plan names "rustls with the Mozilla CA bundle".

ring builds C and assembly through the cc crate. Its build script picks
the assembly by the operating system, from a list of those with Linux's
ELF and calling convention, and its `SystemRandom` takes bytes from the
getrandom crate only on the systems it lists; Radix is in neither list.
Radix's programs are ELF with the System V convention, as Linux's, and
the rust-crates port (ADR 0080) already patches getrandom to take bytes
from getentropy.

A TLS client checks a server's certificate against the root
certificates it trusts. Programs from Linux read them from a file:
Debian's `/etc/ssl/certs/ca-certificates.crt`, which curl and Go look
for, and Alpine's and the BSDs' `/etc/ssl/cert.pem`. Mozilla's list,
which Debian's ca-certificates is made from, is the one most of the free
world trusts; the Common CA Database (CCADB) publishes it under the
Community Data License Agreement - Permissive 2.0, and the rustls
project's webpki-root-certs crate carries it, certificate by
certificate, made from CCADB's report.

## Decision

- **TLS is rustls 0.23 on ring 0.17,** linked into each program in Rust
  that needs it, as Cargo links crates; there is no TLS library of the
  system's. Programs in C get rustls through rustls-ffi (12a.5c, curl).
  OpenSSL is not ported.
- **ring is patched for Radix** in the rust-crates port: two lines add
  `radix` to its build script's list of systems with Linux's ABI, so its
  x86-64 assembly for AES-GCM, ChaCha20-Poly1305, SHA-2 and the
  curves is built, as on Linux,
  and to the systems whose `SystemRandom` uses getrandom, which takes
  bytes from mlibc's getentropy, the kernel's generator (ADR 0022).
- **Mozilla's roots are one PEM file of the system's,**
  `/system/resources/certificates/ca-certificates.crt`, written by a new
  port, ca-certificates, from webpki-root-certs as its git repository
  pins it: each certificate as it is, after its name. Its version is the
  crate's, and a newer list comes with an update of the port.
- **Linux's names lead to it.** The default settings have
  `/etc/ssl/certs/ca-certificates.crt` and `/etc/ssl/cert.pem` (in
  `/system/settings`) as symbolic links to the file, as `/etc/resolv.conf`
  leads to netd's (ADR 0191). An administrator who trusts other
  authorities, or fewer, makes a link a file of their own; programs in
  Rust read the file too, not a list built into them.
- **CDLA-Permissive-2.0 is allowed for ports' data.** It asks only that
  its text go with the data, which the license manifest does; it is not
  in the kernel area's list, as no code is under it.
- **A boot test, `tls`,** runs `tls-test`: ring's hashes, MAC, key
  derivation and sealing against the standards' answers and OpenSSL's,
  OpenSSL's signatures checked, rustls's handshakes over `lo` with every
  version, suite and group it offers on ring and every kind of key, what
  a client must refuse, and the system's roots. The certificates, keys
  and signatures it uses were made by OpenSSL (`testdata/make.sh`), so
  that both ends of a check are not the code under test.

## Consequences

- Programs in Rust have TLS 1.2 and 1.3 with AES-GCM and
  ChaCha20-Poly1305, X25519, P-256 and P-384, and ECDSA, Ed25519 and RSA
  certificates; ring offers no post-quantum key exchange, which waits for
  rustls's ring provider or a later choice.
- Each program that uses TLS carries its own copy of rustls and ring,
  about a megabyte, as static linking does for every program in
  `/system` (ADR 0048); an update of either rebuilds them.
- Mozilla's list has no word in a PEM file for the limits it puts on some
  authorities, such as the dates after which it no longer trusts what
  they issue; as on Linux, those authorities are trusted for all they
  issue until Mozilla takes them out of the list.
- Nothing yet adds an authority to the list without replacing the whole
  file, as update-ca-certificates(8) does on Debian; certd (Phase 13)
  and the admin tools can bring one.
- The `tls` test found a kernel bug from 12a.2b (ADR 0183). mlibc's
  `accept` names the lowest free descriptor before it waits, and a
  thread that opened one while another waited took it first; the kernel
  then closed the connection `accept` had taken, and `accept` waited for
  the next one, so the client saw its connection end. The kernel now
  gives such a connection back to the listening socket, at the front of
  those that wait, as it did for a Unix-domain socket's
  (radix-file(2)), and `accept` takes it into the next free descriptor.
- Tests: `tls-test` in the `tls` boot test, the `posix` test's `inet`
  part for the `accept` fix, and ob's archive test checks the links.

## Alternatives considered

- **aws-lc-rs**, rustls's default: faster RSA, and ML-KEM, but a CMake
  and C build of AWS-LC for Radix, and a second crypto core beside the
  ring that boringtun needs.
- **webpki-roots built into each program**, as many Rust programs do:
  the list would change only when each program is rebuilt, and an
  administrator could not change what is trusted.
- **Mozilla's certdata.txt, as Debian converts it:** the same
  certificates, under the Mozilla Public License, from NSS's repository,
  with a converter of our own for its format.
- **OpenSSL or LibreSSL as the system's TLS library:** C code that
  parses what any network sends, which rustls's memory safety avoids.
