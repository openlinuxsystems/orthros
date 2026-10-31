<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0221. Bundle manifests, version 1: strict JSON, read by the manifest crate, with a schema made from the same types

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13d.1. A bundle is an app, a command-line tool or a service with
everything it needs, in one folder or one `.dkv` file (Chase,
2026-10-29: a bundle is one zip, and `.dkv` is its extension). Its
`manifest.json` is the only thing Dukovo reads to install it, show it,
or decide what it may do. The security design's sections 11 and 12 set
what version 1 holds and the rules for changing it:

- **Exactly what 1.0 uses:** identity, programs, commands, services,
  permissions, data, interfaces and updates.
- **Strict:** within a version an unknown field is an error, except
  `x-` fields, which are a vendor's own.
- **Add only:** a later version adds fields and never changes what one
  means. A manifest newer than the system is refused with a message to
  update Dukovo.
- **Readable forever, not granted forever:** Dukovo may narrow what an
  old permission gives, or ask again, to protect people.
- **Reserved:** `entries`, `actions`, `display`, `runtime` and `plugins`,
  and the permissions of later releases, are names version 1 keeps for
  the version that defines them.
- **Ship before freezing:** version 1 is a preview until 1.0.0.
- **One schema** (JSON Schema) for the reader's tests, the fuzzing, the
  man page and `bundle upgrade-manifest`.

ADR 0216 put service files in the `manifest` crate so that Spark,
`service` and `ob` read them alike, and left the bundle manifest's
reader, its schema and a fuzzer to this part.

## Decision

- **`manifest::bundle`** reads manifests, behind the crate's `bundle`
  feature, with `serde_json` 1.0 without the standard library. Spark
  takes neither feature, so it does not grow. `systools` takes `schema`,
  which adds the JSON Schema; `ob` takes `bundle`.
- **Strict reading, with lines.**
  - The top level is read field by field: an unknown field is an error
    that says manifest.json(5) lists them; an `x-` field and `$schema`
    are read past; a field written twice is an error; a reserved field
    says a later version brings it.
  - Each nested object denies unknown fields, and every map refuses a
    key written twice, which serde_json alone would let the last win.
  - `manifest_version` is looked at first, so that a manifest of version
    2 is refused as one ("update Dukovo to install it") and not for the
    first field version 1 does not know.
  - An error has its line: serde_json's for what reading finds, and for
    the checks across fields, the line of the key or element they are
    about, found by a small scanner of the already-read text (`locate`).
    Missing fields are said without a line, which would be the end of
    the file. Errors are in plain words, as service files' are.
- **Nothing from a manifest reaches a terminal as it is.** Texts and
  paths hold no control characters, and an error that quotes the
  manifest, such as an unknown field's name, writes each one as an
  escape, so a downloaded bundle cannot write to the terminal that
  `bundle check` prints to.
- **Every text is checked as it is read.** The ID is reverse DNS in
  lowercase; a version is one to four numbers with semantic versioning's
  pre-release and build; `requires_os` is a Dukovo release; addresses are
  `https://`; paths stay inside the bundle (no leading `/`, `.`, `..` or
  empty part); names, accounts, interfaces (`name/MAJOR`), language tags
  and minisign keys each have their form. The license is an SPDX
  expression, parsed (`AND`, `OR`, `WITH`, parentheses, `LicenseRef-`),
  not looked up in a list, so a new SPDX identifier needs no update.
- **The checks across fields:** each program has a path for each
  architecture and no other; a program narrows itself only to permissions
  the bundle asks for; commands and services run programs it has, under
  names of their own; a service without `account` has a name that can be
  an account's, and one that starts on a connection provides a socket; a
  health check's socket is one it provides; nothing is named twice in
  `needs`, `uses` or `apps`; each `provides` names a socket or an
  interface, one of the two; a GNU
  license (GPL, LGPL, AGPL) needs `source`; the icon is an SVG;
  translations explain only permissions it asks for. A permission without
  `why` is a note, not an error.
- **The permission list is data** (`bundle::permissions::LIST`): each
  permission's name, the words the system shows, which an app cannot
  reword, its tier (at install, on first use, with a warning, by an
  administrator, always unless turned off) and what it takes besides
  `need` and `why`: your folders, folders of `/shared`, ports, or other
  apps' IDs. Version 1 defines the 27 that arrive before 1.0; the other 21
  are reserved with the release that brings them, and a manifest that
  asks for one is told which. The tiers decide the prompts of 13d.4.
- **Services take a service file's settings** (`service.toml(5)`), with
  the program by name and the same messages, and add `account`, `start`,
  `needs`, `health` and `limits.open_files`. `open_files` goes up to
  2^20, Linux's `fs.nr_open`: a process holds no more descriptors than its
  capability space has slots for, so a program written for Linux may ask
  for what it asks for there. The bundle's runtime sets it (13d.3), since
  a process sets only its own limits.
- **The JSON Schema is made from the same types**, with `schemars` 1.2
  (MIT, without the standard library). A schema-only struct holds every
  field and adds `$schema` and `x-` fields; the permissions' schema is
  made from the list, each with what it takes. A unit test checks that
  the schema has exactly the fields and the defined permissions the
  reader has. `bundle schema` prints it, draft 2020-12, with `$id`
  `https://dukovo.org/schemas/manifest-1.json`.
- **`bundle`** (bundle(1), in systools): `new ID [FOLDER]` writes a
  manifest that reads, with one program and its command, and a `bin`
  folder; `check [FOLDER | FILE]` prints each error as `FILE:LINE: why`
  and, for a folder, each program or icon that is not there or may not be
  run; `schema`; `upgrade-manifest`, which has nothing to do until
  version 2. `new`'s license is `LicenseRef-Proprietary` unless given,
  and it says how to choose another: Dukovo does not choose a license for
  anyone.
- **The design's worked examples are the tests' fixtures**: nginx,
  PostgreSQL, Git and Irssi read; its Node.js and Photos manifests, which
  use what later versions bring, are refused as version 2. A seeded fuzz
  test damages them 4000 ways and checks that the reader never panics and
  that every error's line is in the text.

## Consequences

- One reader for every tool: `bundle check`, installing (13d.3) and the
  catalog (13e) say the same thing about the same manifest.
- `./ob docs --check` checks that manifest.json(5) names every field,
  reserved field and permission the crate knows.
- systools grows by serde_json, which it had, and schemars.
- Version 1 is a preview: until 1.0.0 a field may still change, and
  `upgrade-manifest` will carry manifests across.
- **Not yet:** signing and the `.dkv` file, with `bundle pack` (13d.2);
  installing and removing, and the runtime that applies `open_files`
  (13d.3); granting permissions and the trusted prompts (13d.4); a
  published copy of the schema at its `$id` (with the web site); version
  2's `entries`, `actions` and `display` (Dukovo 1.1 and 1.5).
