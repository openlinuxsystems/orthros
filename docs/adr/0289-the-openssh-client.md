<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0289. The OpenSSH client

- Status: Accepted
- Date: 2026-11-01

## Context

Most git remotes are reached over ssh (`git@github.com:...`), and the
git support plan's fourth step is the client that reaches them. git
2.55 (ADR 0287) runs `ssh` for an `ssh://` or `host:path` remote, and
signs commits and tags with an ssh key when `gpg.format` is `ssh`.
Signing in to another computer, copying files to it and keeping keys
for a session are what the same client is for anyway.

OpenSSH is the client nearly every system ships. It can be built
without OpenSSL: its own code then has Ed25519 keys and signatures,
ChaCha20-Poly1305 and AES-CTR/GCM ciphers, and the ML-KEM768 and
sntrup761 hybrid key exchanges with X25519, which are what OpenSSH
itself picks first. RSA and ECDSA keys need OpenSSL. Dukovo has no
OpenSSL; its TLS is rustls (ADR 0192).

## Decision

**OpenSSH 10.6p1's client is a port**, in every image, built without
OpenSSL from the release's commit in openssh-portable, which GitHub
serves (cdn.openbsd.org is out of reach of the build containers).
`ssh`, `scp`, `sftp`, `ssh-add`, `ssh-agent`, `ssh-keygen` and
`ssh-keyscan` go in `/system/commands`, with their manual pages and
ssh_config(5); upstream's `ssh_config`, all comments, goes among the
default settings, so it is `/system/settings/ssh/ssh_config`. There is
no `sshd` in this step: the server stays Phase 13c's, on russh,
managed by Spark and signing in through authd. 13c planned the `ssh`
and `sftp` clients from russh too; OpenSSH's are the clients instead,
since they are what git, guides and people's habits expect, and 13c
needs to build only the server.

It is built without PAM, Kerberos, libedit, ldns, security keys
(libfido2) and PKCS#11, which loads libraries. Ed25519 is the only key
type, so RSA and ECDSA keys and hosts that offer only those are not
supported; `ssh-keygen` makes Ed25519 keys by default. A FIDO key or
RSA is for a later step with a cryptography library of Dukovo's
choosing.

Two changes to the build: `xcrypt.c` calls crypt(3), which mlibc does
not have and only a server needs, so a patch leaves it out; and
mlibc's getopt(3) moves options after the first argument to the
front, as glibc's does, which would make `ssh host ls -l` take `-l` as
ssh's. OpenSSH uses its own getopt where optreset is missing, as it
does on glibc, so the build tells configure it is missing.

OpenSSH's licence is several: the `SSH-OpenSSH` and `SSH-short`
notices, BSD-2-Clause, BSD-3-Clause, ISC, the `ssh-keyscan` and
`snprintf` notices, X11-distribute-modifications-variant, IBM's base64
notice, and public domain code (chacha, poly1305, curve25519, ed25519,
sntrup761, Rijndael). `SSH-OpenSSH`, `ssh-keyscan`, `snprintf` and
`LicenseRef-OpenSSH-public-domain` join the port allow-list; each is
permissive.

## Consequences

- The `ssh` boot test signs in to a real OpenSSH server: QEMU's user
  network runs the host's `sshd -i` for each connection to 10.0.2.100.
  It makes keys, signs a file and a git commit with one, keeps it in
  ssh-agent, takes the host key and refuses a changed one, copies
  files with scp and sftp, and clones a repository over ssh, commits
  and pushes, which the host checks. The build host needs
  `openssh-server` and Autoconf (`ob doctor` checks both).
- git's `ssh://` remotes and ssh-signed commits work.
- Hosts that offer only RSA or ECDSA keys cannot be signed in to.
