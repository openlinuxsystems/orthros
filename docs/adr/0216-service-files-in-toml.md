<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0216. Service files are TOML, read by one crate, and a file that does not read never takes a service down

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.4. Since Phase 9 Spark has read service files of `key =
value` lines (ADR 0101), which grew `memory`, `oom`, `weight` and, in
12a.2, `network.outbound` and `network.listen` (ADR 0183). The security
design's section 8 and its decision 7 (Chase, 2026-10-26) settle the
format: TOML for the files people edit by hand, JSON for a bundle's
`manifest.json` and for anything tools write, one schema for both, so
that a service in `/system/settings` and a bundle's service read the
same and use the same permission names. Chase's condition is that both
be easy to use, so the tooling ships with the format: one set of Rust
types that every reader shares; the same parser everywhere; a mistake
never takes a service down, and Spark says which line is wrong and why
in plain words; `service edit` checks before anything changes, as
visudo does; `service check` and `service new`; files that explain
themselves, with nano highlighting TOML; today's files converted with
their comments; and CI that checks every shipped file.

13b.4 also gives each service the capabilities its file declares, in a
view (ADR 0215), and its secrets as read-only files. That half is 13b.4b
(ADR 0217); this ADR is the format.

## Decision

- **One crate reads service files**, `manifest`
  (`userspace/programs/manifest`), with no standard library, so that
  Spark, the `service` command and `./ob` share its types and its
  messages: `service check` says what Spark will say. It reads TOML with
  the `toml` crate's 0.9 line, which builds without the standard library
  (`parse` and `serde` features only; MIT OR Apache-2.0), and `serde`
  (MIT OR Apache-2.0). The bundle manifest's `services` section takes
  the same types in 13d.1, which also generates the JSON Schema from
  them with schemars.
- **The format** (service.toml(5)): `NAME.toml` in
  `/system/settings/spark/services`; keys in snake_case; top-level
  `description`, `program` (an absolute path, required), `arguments` (an
  array), `environment` (a table), `order` (50 unless set), `enabled`
  (true unless set) and `restart` (`"always"`, `"on-failure"` or
  `"never"`); `[limits]` with `cpu_weight` (1 to 10000, 100 unless set;
  the old `weight`), `memory` (bytes, `"64M"` and so on, or `"none"`)
  and `oom` (`"service"` or `"app"`); and `[permissions]` with the
  security design's permission names, `network.outbound` and
  `network.listen` (ports and `"first-last"` ranges, none overlapping,
  at most eight grants in all). The names are the permission list's, so
  a bundle's service asks for the same words.
- **Versioned.** Every file starts with `service_version = 1`, as every
  manifest starts with `manifest_version` (Chase, 2026-10-24): a later
  Dukovo reads every earlier version, and a file for a newer one is
  refused with a message to update Dukovo.
- **Strict, and plain.** A key the format does not know is an error, so
  a misspelt setting is never silently ignored. Each error names its line
  and says what is wrong in words, not in the parser's: "line 6: restart
  must be always, on-failure or never, not `sometimes`", "`weigth` is not
  a setting here; service.toml(5) lists them".
- **The last good copy.** Spark reads a service's file at boot and again
  each time it starts the service, so a changed file takes effect at the
  next start, CPU weight and memory limit included; the out-of-memory
  class stays the domain's until the next boot. Each file that reads is
  kept in `/system/state/spark/good`. A file that does not read never
  takes a service down: Spark logs the line and the reason once, ending
  "it runs its last good copy", and starts the service from that copy; a
  service whose file never read is not started. `service NAME start` of
  a service Spark does not know reads its file, so a service added since
  boot starts without a reboot.
- **Converting.** Spark converts each `NAME.service` with no `NAME.toml`
  beside it, at boot before the defaults are seeded and when its service
  is started: every comment stays above the setting it was above, values
  gain their quotes, `weight` becomes `[limits]` `cpu_weight`, and the
  network keys go in `[permissions]`. The result is read back before it
  is written; the old file is kept as `NAME.service.old`. A file with an
  error is not converted, and Spark logs why.
- **The `service` command** (service(8)): `service check [NAME | FILE]...`
  checks files, by default every one; `service new NAME` writes a
  template with every setting explained, `enabled = false`; `service
  edit NAME` works as visudo: it copies the file to `.NAME.toml.edit`
  beside it (made exclusively, so two edits never cross), runs
  `$VISUAL`, `$EDITOR` or nano on the copy, checks it, and renames it in
  place only once it reads; otherwise it shows the error and asks "(e)dit
  again, or e(x)it without saving".
- **nano highlights TOML** with a syntax file of Dukovo's own,
  `/system/resources/nano/toml.nanorc`, since nano has none; it also
  matches the `.edit` copy.

## Consequences

- ADR 0101's `key = value` format gives way to this one; its
  supervision, domains and control socket stay as they are.
- Every shipped service file is TOML with its settings explained in
  comments, and the manifest crate's tests read each of them, the
  template, every setting and every old setting converted, so a shipped
  file that does not read fails `./ob test unit`. A test breaks each
  shipped file at every byte, deleting it or putting a quote, a bracket,
  a line break or an `=` there, and checks that the reader never panics;
  a fuzzer comes with the bundle manifest's parser (13d.1), as the design
  asks one for both.
- `./ob docs --check` checks that service.toml(5) explains every key the
  crate knows.
- The new `services` boot test checks every shipped file with `service
  check`, converts a file of before Phase 13 as its service starts, writes
  one with `service new` and edits it with `service edit`, broken, given
  up on and fixed, starts a service added since boot, and breaks a file
  under a running service, which keeps running from its last good copy.
- Spark grows by the `toml` and `serde` crates. A service's start reads
  one small file more than it did, which no benchmark row measures.
- **Not yet:** the capabilities a service declares, in a view, and its
  secrets (13b.4b, ADR 0217); the JSON Schema and the manifest's
  `services` section (13d.1); comments kept by Dukovo Admin's editor
  (toml_edit, 16c); dependencies, health checks and timers (16a).
