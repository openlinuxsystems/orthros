<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0223. bundled installs bundles, seals them, and removes them

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13d.3. A bundle has a manifest (ADR 0221), a signature over a
list of its files, and travels as a `.dkv` file that a confined
unpacker opens (ADR 0222). The security design (sections 9 and 10,
locked by Chase on 2026-10-26) asks that:

- a bundle installs for everyone in `/applications`, by an
  administrator through elevate, or for oneself in `~/applications`;
- installing checks the signature and seals the bundle, so that nothing
  changes it after: "nothing in that folder is ever written after
  install";
- a bundle's ID is bound to its publisher's key the first time it is
  installed, and a later version must carry the same key;
- a bundle signed by a developer key outside any catalog installs after
  a warning that shows the publisher and the permissions; one signed by
  the developer key `bundle new` made installs on that machine with no
  warning;
- removing asks each time whether to delete the app's settings and
  data, and `--keep-data` and `--purge` answer for scripts;
- the bundle command does for bundles what apt and dpkg do on Ubuntu:
  `install`, `list`, `info`, `remove`.

Sealing a folder in a person's home means files that person cannot
change, so something other than the person must write them: root.

## Decision

- **bundled(8)**, the bundle runtime, is a service acting for root
  that installs and removes bundles for everyone who asks it, on
  `/system/live/bundled/socket`. It knows who asks by the user the
  kernel gives for the connection, as authd does (ADR 0218). It keeps
  what it installed in `/system/state/bundled`, which only it writes:
  - `installed`: each bundle's place, ID, version, key, the SHA-256 of
    its signed list, and when;
  - `bound`: each ID's key, for everyone and for each person, from its
    first install there;
  - `developer-keys`: each person's developer key, which `bundle new`
    and `bundle key` tell it.
- **Places.** A bundle for everyone is `/applications/ID`; one for a
  person is `ID` in their home's `applications` folder. Installing for
  everyone is root's, so an administrator runs `elevate bundle install
  --for-everyone`.
- **Installing** (`bundle install FILE.dkv`): bundle opens the file and
  passes it to bundled (SCM_RIGHTS). bundled:
  1. opens the place's folder, not through a link, and checks that it
     is a folder of the asker's (or root's, for everyone), so a link or
     a folder of someone else's never leads it elsewhere;
  2. makes a folder of its own there, `.installing-RANDOM`, mode 0700,
     and unpacks the archive into it with the confined unpacker (ADR
     0222), which holds only the archive and that folder;
  3. checks the unpacked folder again itself: the signature, the list
     and every file's SHA-256, and that it holds nothing but folders
     and regular files, since the unpacker read what came from
     anywhere;
  4. decides whether to trust the key (below), and refuses a command
     named as one in `/system/commands` or one another bundle in reach
     already adds;
  5. answers with the **install sheet**: the name, ID, version and
     publisher, the key and why it is trusted or not, the size, the
     permissions it asks for in the system's own words with how each is
     given, and the commands and services it adds. A bundle whose key
     this computer does not know shows a warning. bundle shows the sheet
     and asks; `--yes` answers for scripts;
  6. seals it: each file root's, mode 0444, or 0555 for a program, and
     each folder root's, 0555, so the person can neither change nor add
     a file;
  7. renames it to its ID, replacing a version of the same ID from the
     same key, whose folder it then removes;
  8. records it, binds the ID to its key at its first install there,
     and writes a security event to the audit log: `alice installed
     org.example.notes 0.1.0 for herself, signed by key RWQ... (her
     developer key)`.

  Anything that fails removes the folder it made, and nothing is
  recorded.
- **Trust**, until catalogs (13e):
  - a key bound to the ID: the same ID signed by any other key is
    refused, as an update would be, saying since when it is bound; a
    person's install also keeps the ID's binding for everyone;
  - the asker's developer key: trusted for their own installs, with no
    warning, as Xcode's "Sign to Run Locally"; another person's
    developer key is like any other;
  - any other key: installs after the warning, and is bound from then.
  - A bundle without a signature never unpacks (ADR 0222).
- **Listing and showing:** `bundle list` lists what the asker can run:
  their own bundles and everyone's, with version, place and key.
  `bundle info ID` shows the install sheet of an installed one, where it
  is, when it was installed, and its data folder.
- **Removing** (`bundle remove ID`): bundle asks "Also delete its
  settings and data?", Keep or Delete, unless `--keep-data` or `--purge`
  answers. bundled removes the sealed folder and the record; Delete
  removes the asker's `~/app-data/ID` too. The ID stays bound to its
  key, so a later version from the same publisher picks kept data up
  and one from anyone else does not. `--for-everyone` removes from
  `/applications`, through elevate; each person's data stays.
- **Noticing.** At its start bundled forgets a recorded bundle whose
  folder is gone, as when it was deleted while the system was off.
- **Developer keys.** `bundle new` and `bundle key` give bundled the
  person's developer public key, which it keeps for them alone.

## Consequences

- A person installs and removes their own bundles without elevate, and
  cannot change one once it is installed; an administrator installs for
  everyone through elevate.
- A bundle's folder that is not root's, or whose signed list is not the
  one recorded, was not installed by bundled, so the runtime (13d.3b)
  does not start it.
- Two people may install the same ID from the same key; from different
  keys, only if neither is bound for everyone.
- **Not yet:** catalogs, their keys and TUF metadata, updates side by
  side and rollback (13e); key rotation signed by the old key (13e);
  the per-bundle override for unsigned bundles; read-only Btrfs
  subvolumes for sealed bundles, and sharing identical files through
  reflinks; asking each person about their data when an administrator
  removes a bundle for everyone, at their next sign-in; the Trash for
  deleted data, which comes with the desktop; installing by dropping a
  file into `~/applications`, which needs a watcher (Phase 14); grants
  and the trusted prompts (13d.4).
