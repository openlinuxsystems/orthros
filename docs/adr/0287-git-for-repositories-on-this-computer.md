<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0287. git, for repositories on this computer

- Status: Accepted
- Date: 2026-11-01

## Context

Dukovo's terminal is to be the best place to use git (the git support
plan, approved 2026-11-01): a prompt that shows a repository's state,
completion that knows branches and changed files, fixes and warnings
before what cannot be undone. All of that needs git itself, and the
image had none: Phase 13e planned git as a catalogue bundle and
Phase 21 among the developer tools.

git needs Perl, Python and Tcl/Tk only for a few commands (`git svn`,
`git p4`, `git send-email`, gitk, git gui), gettext only for
translations, libcurl for `https://` remotes and OpenSSL only where
curl or a hash would use it. Its manual pages are made from AsciiDoc,
which the build host does not have. Its Rust part is still optional in
2.55, and becomes required in git 3.0.

## Decision

**git 2.55.0 is a port**, in every image, under version 2 of the GPL
only, as a program of its own (ADR 0087). `GPL-2.0-only` joins the
port allow-list. It is built from the release archive kernel.org
serves, with Ubuntu's source pool as the mirror, and its manual pages
come made already from git's own repository of them,
gitster/git-manpages, pinned to the commit for v2.55.0.

**This first step is for repositories on this computer**: `init`,
`commit`, branches, merges, `stash`, `rebase`, `bisect`, `submodule`,
`gc` and the rest, and remotes by a path or `file://`, which run
`git-upload-pack` and `git-receive-pack` as another computer would.
It is built without curl, Perl, Python, Tcl/Tk, gettext and Rust. The
`https://` side comes with libcurl on rustls (the Phase 12 port it
shares), and `ssh://` with the OpenSSH client, as later steps of the
plan. The commands that need Perl or Python say they are not built in.

**Where it goes** (radix-files(7)): `git` in `/system/commands`,
without a `git-<name>` link for each built-in command, only the three a
remote runs by name, and `git-shell`; the helpers and shell scripts in
`/system/libraries/git-core`, as Debian keeps them in `/usr/lib`; a new
repository's templates in `/system/resources/git-core`. gitweb, the
HTTP backend, `imap-send`, `scalar`, the CVS server and bash's
completion are left out, and with them their manual pages.

**git reads `/system/settings/gitconfig`**, which Spark copies from
the default settings, as it does nano's. It sets what most guides tell
a new user to set by hand: `main` as a new repository's first branch,
without the hint; `help.autocorrect=prompt`; moved lines shown as
moved and the histogram diff; `zdiff3` conflicts, which show the text
both sides started from; `rerere`, so a conflict resolved once is
resolved the same way again; `fetch.prune`; `push.autoSetupRemote`, so
a new branch's first push needs no `-u origin`; branches by their last
commit and tags by version. Each user's `~/.gitconfig` and each
repository's own change any of it. It does not set the editor:
sign-in gives `EDITOR=nano`, which git uses unless told otherwise.

mlibc has no iconv, no `strlcpy` and no `REG_STARTEND` in its regular
expressions, so git builds its own `strlcpy` and regular expressions
and works without iconv: commit messages in encodings other than UTF-8
are not re-encoded.

## Consequences

- The `utilities` part of the posix boot test runs git as a person
  and a script do: commits whose hashes it checks, with the names and
  times set, branches, a merge, a conflict undone, tags, `stash`,
  `grep`, `blame`, a clone over `file://`, a push into a bare
  repository, `gc` and `fsck`, `submodule` and `bisect run`, and the
  programs another computer runs by name, through `git-shell` too.
- The image grows by about 7 MiB, half of it git and half its manual
  pages, which `git help` and Nukesh's explain read.
- Nukesh's git features, the next step of the plan, can call git
  where reading `.git` itself is not enough.
- git 3.0 will need its Rust part built for Radix, as uutils is.
