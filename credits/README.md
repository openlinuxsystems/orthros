<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Credits added one file each

A pull request that starts using a new project, or draws on another
project's design, credits it in a file of its own here, `<project>.md`,
instead of adding a row to [CREDITS.md](../CREDITS.md): rows added at the
end of the same section conflicted between pull requests built at once.

After the two SPDX comment lines, the file's first line names the section
of CREDITS.md the credit belongs to, and the rest is what that section
would hold, usually one table row:

```markdown
Section: In every image
| [Example](https://example.org) 1.0 by Its Authors | What we use | MIT |
```

`ob docs` writes CREDITS.md with these in place to `out/docs/CREDITS.md`,
and fails if a file names no section CREDITS.md has.
