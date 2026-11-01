<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0268. Explain this command, with Alt+E and `explain`

- Status: Accepted
- Date: 2026-11-01

## Context

Chase approved the Nebix "Explain this command" design (design draft, 2026-11-01): Alt+E, a pipeable `explain`, warnings first, secrets masked, local only, never runs anything, a plain answer from the manuals when Nebix is not running.

## Decision

- **Alt+E** prints what the line does under it, and the prompt is drawn again below with the line as it was. On an empty line it says why the last command failed: its status and what that status usually means.
- **`explain [command ...]`** prints the same; with no arguments it explains each line of its input.
- **The plain answer:** each program's line from its manual page (`.Nd`, or the line after `.SH NAME`), each option's line from its completion file (ADR 0102), short groups (`-xzf`) split letter by letter with the argument they take, builtins' own summaries, aliases through what they run, redirections, and how `|`, `&&`, `||`, `;` join the parts.
- **Warnings first, in bold:** deleting (`rm`, `shred`, `git clean -f`), overwriting (`>`, `dd of=`, `mv`, `cp -f`), formatting and partitioning, permissions and owners (worse with `-R`), running as the administrator, `curl|sh`, sending data away (`scp`, `rsync host:`, `curl -T/-d/-F`, `nc`, `git push`), rewriting history (`git push --force`), and power.
- **Secrets are masked** before anything is shown or asked: values after `--password`/`--token`/..., variables whose names hold PASSWORD, TOKEN, SECRET, KEY..., and the password in `scheme://user:pass@host`.
- **Nebix:** if `/system/live/nebix/socket` is there, it is asked, a JSON line each way (`{"explain": line, "status": n}`), with 1.5 s at most; its answer follows the plain one. Nebix lands in 1.10; until then the plain answer stands alone. Nothing leaves the computer.
- After a failure, the empty line offers it: `Alt+E explains what went wrong`, faded; `[nebix] offer = false` turns that off.

## Consequences

- Explanations are not kept in the history or the record.
- The plain answer needs no network and no model, and costs a few file reads.
