<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0288. git in Nukesh

- Status: Accepted
- Date: 2026-11-01

## Context

With git in the image (ADR 0287), the plan approved on 2026-11-01 asks for
the terminal to be the best place to use it: better than zsh with
oh-my-zsh, fish, Starship and Warp, with nothing set up and nothing sent
off the computer. It also asks for seven helpers that make git clearer:
an account of a repository's state, guardrails, undo, tags, conflicts,
tidying and a commit sheet. Each must show the real git command:
nothing hides or replaces git.

Before this, Nukesh's right prompt showed the branch, read from
`.git/HEAD`, and a `*` when `git status` found changes within 100 ms.

## Decision

**A git module in Nukesh** (`git.rs`). It reads what can be read from the
repository's files without running git: the branch, a detached HEAD's
tag (lightweight tags and `packed-refs`, whose peeled lines give an
annotated tag's commit) or short hash, an operation under way (rebase
with its step, merge, cherry-pick, revert, bisect, am), and the stashes.
The counts of files, and how far the branch is ahead of and behind its
upstream, come from `git status --porcelain=v2 --branch`, run on a
thread of its own. The prompt waits for it 500 ms at most (Starship's
`command_timeout`; `[git] status-wait` changes it), and otherwise
shows the last count it found for that work tree, so that a big
repository never holds the prompt up. The plan named an in-process
reader of the index. A reader that matches git's own (the index
extensions, untracked-cache, `.gitignore` rules, packs and deltas,
reftable, sparse checkouts) is a project the size of libgit2, and
it would drift from the git that is installed. A background `git
status` gives git's own answer and the same never-wait prompt, which is
how Starship and powerlevel10k's gitstatusd behave in practice.

**The prompt** shows, at the right: the operation (`REBASING 2/5`), the
branch, or the tag or commit when detached; `↑2 ↓1`; `!1 conflict`;
`+3 ~2 ?1` for staged, changed and untracked; `≡1` for stashes; and `✓`
for a clean work tree. Each part has a colour key in colors.toml
(`git-state`, `git-sync`, `git-conflict`, `git-staged`, `git-changed`,
`git-untracked`, `git-stash`, `git-clean`). The console's font
(Spleen) lacks `⎇` and, at some sizes, the arrows, so with `TERM=linux`
the prompt uses `git`, `^`, `v` and `stash` instead.

**Tab** completes git's commands with what each does, from their
manual pages, and the aliases in the settings. It completes branches with
their last commit and its age, a remote's branches you have no branch
of (which `git switch` makes one of), tags, remotes, stashes, and for
`git add` and `git restore` only the files that changed, those in
conflict alone while there are any. For `git tag` it offers the next
patch, minor and major versions after the newest, as an annotated tag
(`-a v1.2.4 -m "Version 1.2.4"`). Each command's options come from a
completion file that the build writes from its own page (`git-switch`
for `git switch`), as it does for every program (ADR 0102).

**Corrections and the next step** (ADRs 0280, 0285): a mistyped command,
a first push without an upstream, a branch one typo away, a commit with
nothing staged, and a pull that cannot merge on its own each get the
corrected command on the next line. A command that worked gets its usual
next step when the record has none: a commit after `git add`, a push
after a commit, `git push --follow-tags` after a tag, and
`git branch -d` for branches already merged into the main branch after a
pull or a switch to it. The `--continue` of a rebase, merge or
cherry-pick, once the last conflict is added, comes before habit.

**Alt+E** (ADR 0268) on a git command puts this repository's numbers in
its warnings: the files a hard reset or a restore throws away and the
commits it moves past, the commits a forced push replaces (with
`--force-with-lease` offered), the files a clean deletes (from `git
clean -n`), and the commits a branch deleted with `-D` takes with it. It
names the undo of each command. On an empty line in a repository, it
says in plain words where HEAD is, what is under way, what has changed
and the next step.

**Guardrails and reminders**: before `git commit` runs, a note when
HEAD is detached, as the commit would be on no branch, and a quiet one
when it goes straight to the main branch of a repository with a remote.
On entering a repository, once a session, a note of stashes a month
old. `[git] guardrails` and `[git] reminders` in settings.toml turn them
off.

**The commit sheet**: Alt+C shows, under the line, the staged files, a
Conventional Commits prefix that Tab chooses (or none), and the subject
with a count of its letters, which turns yellow past 50 and red past 72.
Alt+N asks Nebix, on this computer, for a draft from the staged diff.
Alt+A takes every changed file. Enter puts `git commit -m '...'` on the
line, to run or not. It never commits by itself. Alt+C was reedline's
capitalise-word key; Alt+U and Alt+L still change a word's case.

**Repositories elsewhere in the shell**: Ctrl+R ranks commands run
anywhere in the current repository just after those run in the current
folder. `z` weighs a repository's top folder half again, so `z site` goes
to the repository rather than a folder inside it. The new builtin `cdr`
goes to the repository's top folder, or to the superproject's from a
submodule's top.

**Abbreviations**: `[git] abbreviations = true` turns on 24 of the
oh-my-zsh git plugin's abbreviations (`gst`, `gco`, `gp`, `gl` and the
rest; their names are MIT, from ohmyzsh/ohmyzsh). They are off by default,
as the plan recommended. An abbreviation the person defines wins.

## Consequences

- The `shell` part of the posix boot test types at a pseudo-terminal
  through all of this: the prompt, Tab, a fix, Alt+E's warning and
  account, the commit sheet, a tag's versions, `cdr`, the detached-HEAD
  note, and a merge's conflict and `--continue`.
- Each prompt in a repository runs `git status` once, in the
  background. `[prompt] git-dirty = false` leaves it out, and the prompt
  then shows only what the files say.
- The man pages of the image gain about 140 completion files, one for
  each git command.
