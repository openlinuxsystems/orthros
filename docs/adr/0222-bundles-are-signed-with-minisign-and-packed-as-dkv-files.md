<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0222. Bundles are signed with minisign over a list of their files, and travel as .dkv files that a confined unpacker opens

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13d.2. ADR 0221 gave bundles a manifest. Before anything installs
them (13d.3), a bundle needs a signature, a single file to travel as, and
a safe way to be opened. The security design (section 10, and rows 7 and
21 of its third look, accepted by Chase on 2026-10-26) asks for:

- **A signature over the manifest and every file's SHA-256**, Ed25519,
  minisign-compatible. The publisher's key comes from the signature,
  never from the manifest's own claims. Whether to trust a key is the
  installer's question: a catalog's, a key bound to the bundle's ID at its
  first install, or a developer key only this machine trusts.
- **One file, a zip underneath**, with the extension `.dkv` (Chase,
  2026-10-26 and 2026-10-29). The signature covers every file inside, so
  the folder and the file verify the same way.
- **An SBOM in every bundle**, in SPDX as /system's own.
- **An unpacker that holds only the new folder's capability**, refuses
  links, device files and set-ID bits, caps the total size and the
  compression ratio, and parses and verifies in a separate process with
  no other authority, as OpenSSH's privilege separation does.
- **A developer key**, made by `bundle new`, so that a developer's own
  builds install without the unsigned-bundle warning on their machine.

## Decision

- **The signed list.** A signed bundle has `files.sha256`: each of its
  other files' SHA-256 and path, sorted by path as bytes, exactly as
  `sha256sum` writes it, so `sha256sum -c files.sha256` checks a folder.
  The list holds every regular file but itself and `signature`, the
  manifest among them, so signing the list signs everything.
- **The signature is minisign's**, in `signature`: Ed25519 over the
  BLAKE2b-512 hash of the list (`ED`, minisign's way since 0.8), and a
  second signature over the first and the trusted comment, which says
  when and which bundle: `timestamp:T`, `file:files.sha256`, `hashed`,
  `bundle:ID VERSION`. Its untrusted first line is `signed by key RW...`,
  the public key, so a bundle carries the key that signed it and
  `minisign -Vm files.sha256 -x signature -P RW...` checks it as `bundle
  verify` does. The key is trusted for nothing until the installer says
  so; it is only the key the signature proves made it.
- **Keys are minisign's files.** The developer key is a minisign secret
  key without a password, in `$XDG_DATA_HOME/bundle/developer.key`
  (`~/.local/share`), mode 0600, with its public half beside it. `bundle
  new` makes it the first time, and `bundle sign` and `bundle pack` use it
  unless `--key` names another minisign secret key. A key protected by a
  password is refused with how to proceed: reading one needs scrypt, which
  the SDK's signing tool brings (1.6.1). Tests check against minisign
  0.11's own files: its keys and signatures read here, and what is signed
  here with its key is byte for byte what it signed, as Ed25519 is
  deterministic.
- **The `.dkv` file is a plain zip**: each file stored or deflated,
  whichever is smaller, with its Unix mode, the time 1980-01-01 and no
  extra fields; the manifest, the signature and the list first, then the
  list's order. The same folder packs to the same bytes. `unzip` reads it.
- **Reading a zip trusts nothing in it.** The central directory is what
  the archive holds, checked against each local header; entries must
  follow one another with no gap or overlap (a data descriptor, as
  streaming writers leave, is allowed). Refused: encryption, methods other
  than stored and deflate, zip64 (so a bundle is at most 4 GiB and 65,535
  files), split archives, names that are not UTF-8 or not paths inside the
  bundle (`..`, `/` first, `\`, control characters, past Linux's
  PATH_MAX and NAME_MAX), a name twice, a file that is also a folder,
  symbolic links, device files, pipes, sockets and set-ID bits. The files
  may unpack to at most 100 times the archive's size (16 MiB at least),
  checked before any byte is unpacked, and each file's bytes are checked
  against its size, as they come, and its CRC-32.
- **Checking a bundle**, a folder or an archive, reads the signature, the
  list and the manifest, then checks the files against the list: no file
  more, none less, and each one's SHA-256. An archive's are checked as
  they are unpacked, so nothing unchecked is ever kept.
- **The unpacker runs in a process of its own.** `bundle unpack FILE.dkv
  FOLDER` makes the new folder, then starts `bundle unpack --confined`
  with four capabilities and nothing else: the archive (read), a pipe for
  its report (twice, as descriptors 1 and 2), and the new folder (read and
  write, without the bypass right or control), and no root, so no path
  leads out of the folder and nothing else on the system is reachable from
  it. It writes through the folder's capability alone, makes each file
  with `exclusive` and `no_follow`, checks the free space first, and
  reports one strict line: `ok`, the key, the ID, the version, the files
  and the bytes; or `error` and why. The starter believes nothing else
  from it, and removes the folder if it fails. Files are 0644, and 0755
  when the archive marks them executable or the manifest names them as a
  program; modes are not signed, and a changed one gives no authority,
  since the runtime starts only the programs the manifest names.
- **The SBOM** is `sbom.spdx`, SPDX 2.3 tag-value. A publisher writes
  theirs, listing the libraries the bundle carries; `bundle sign` writes
  one from the manifest where there is none, which lists the bundle alone
  and says so, and checks one that is there.
- **Commands** (bundle(1)): `key` shows the developer key, made if there
  is none; `sign [FOLDER]` checks the bundle, as `bundle check` does,
  then writes the SBOM if needed, the list and the signature; `pack
  [FOLDER]` makes `ID-VERSION.dkv`, signing first unless the folder's
  signature already checks; `verify [FOLDER | FILE.dkv]` checks one and
  says whose key signed it and when, and `--key` fails unless that key
  did; `unpack FILE.dkv FOLDER`.
- **Crates**, all from crates.io, permissive, without the standard
  library: `ed25519-compact` 2.6 (MIT), `sha2` 0.10 (MIT OR Apache-2.0),
  `blake2` 0.10, which authd already has, and `miniz_oxide` 0.9 (MIT OR
  Zlib OR Apache-2.0) with `adler2`. Base64 and CRC-32 are a few lines of
  our own.

## Consequences

- A bundle checks the same with Dukovo's tools and with minisign,
  sha256sum and unzip on any other system.
- The installer (13d.3) unpacks with `bundle unpack`'s unpacker, then
  decides whether to trust the key the report names, and seals the
  folder. It needs no zip reader of its own.
- Every byte of an unpacked bundle was checked against its signature,
  so a folder that `bundle verify` passes is the publisher's.
- **Not yet:** trusting keys: the machine's list of developer keys, a
  bundle's ID bound to its key, catalogs' keys, and key rotation signed
  by the old key (13d.3, 13e); keys with passwords (1.6.1); bundles over
  4 GiB, which need zip64; the catalog's checks of SBOMs (13e).
