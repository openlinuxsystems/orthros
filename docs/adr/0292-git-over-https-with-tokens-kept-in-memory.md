<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0292. git over https, with tokens kept in memory

- Status: Accepted
- Date: 2026-11-01

## Context

The git support plan's third step is `https://` remotes: cloning,
fetching and pushing to GitHub, GitLab and the rest the way most
guides show. git 2.55 (ADR 0287) was built without curl. libcurl is
now a port, on rustls through rustls-ffi (ADR 0195), with its headers,
static library and `libcurl.pc` in the sysroot.

A push over https signs in with a token, which git asks for and hands
to a credential helper to keep. The plan named a helper on the user's
keyring, but Dukovo has no keyring yet. git's own helpers are
`credential-store`, which writes tokens to `~/.git-credentials` in the
clear, and `credential-cache`, which keeps them in the memory of a
small daemon of the user's, reached by a socket only the user can
open, for a time it is given.

## Decision

**git is built with libcurl**, its flags from pkg-config against the
sysroot. `git-remote-http` and its `https`, `ftp` and `ftps` links,
`git-http-fetch` and `git-http-backend` (which serves repositories
behind a web server, as CGI) are in the image, with their manual pages.
It is still built without expat, so pushing over the old "dumb" WebDAV
protocol (`git-http-push`) is not built; every forge speaks the smart
protocol.

**The system's gitconfig names `credential-cache` as the helper**, for
8 hours (`cache --timeout=28800`): a token typed once is used without
asking again for a working day, and never written to disk (Chase,
2026-11-01, over a plain-text store or asking every time). A user's
own `~/.gitconfig` can name another helper, or none.

**The keyring comes in Phase 13g**: a keyring for each user, kept by
authd and unlocked at sign-in, with a `git-credential-keyring` helper
that becomes the default in place of the cache. The roadmap holds it.

## Consequences

- The `git-https` boot test runs a smart HTTP server on lo: rustls with
  the network-services test's authority, Basic sign-in with a token,
  and git-http-backend as CGI. git refuses a certificate from an
  authority it does not trust, fails without a token and with a wrong
  one, and, with the token given once, clones, commits and pushes
  without asking; the server's repository has the commit, no file
  holds the token, and git asks again once the cache exits.
- git's programs grow by libcurl and rustls, about 2 MiB each for
  `git-remote-http`, `git-http-fetch` and `git-http-backend`.
- A token is asked for again after 8 hours or a restart until the
  keyring lands.
