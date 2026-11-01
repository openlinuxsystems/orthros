<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0281. Commands holding secrets are kept out of the history and the record

- Status: Accepted
- Date: 2026-11-01

## Context

Commands like `mysql --password hunter2` or `export GITHUB_TOKEN=...` end up in history files in plain text.

## Decision

- `[history] skip-secrets = true` (the default): a command that holds what Explain masks (ADR 0268) is not added to the history or the command record. It still runs, and Alt+E still sees it, masked.

## Consequences

- The history file never holds those passwords and tokens. A command after a space is still kept out too, as before.
