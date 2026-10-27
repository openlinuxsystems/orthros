<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0126. Dukovo never verifies anyone's age or identity

- Status: Accepted
- Date: 2026-10-27

## Context

Dukovo's guiding principle is to favor the user: the user owns the
machine, nothing is forced, and no online account is required
([GOVERNANCE.md](../../GOVERNANCE.md)). Other systems are moving the
other way. Age checks at account setup, birth dates kept with the
account, ID documents or face scans to prove an age, and an age signal
that the system hands to every app are spreading, and some laws now ask
operating systems and app stores for them, such as California's Digital
Age Assurance Act (AB 1043). Each of these collects personal data the
machine does not need, ties an account to a real person, and turns the
system into a checkpoint between its owner and their own computer.

The maintainer made it a hard rule on 2026-10-27: no age or ID
verification.

## Decision

Dukovo never verifies anyone's age or identity.

- No part of Dukovo asks for, checks, estimates or stores a person's
  age, birth date or identity documents: not the installer, the first
  boot, account creation (`user`, the web console, Settings), sign-in
  (authd, login, sshd, Dukovo Admin), the Store and its catalogs, or any
  other part of the system.
- An account is a name the owner chooses, with the password the owner
  decides on after the usual warning and confirmation. Nothing
  ties it to a real person, an email address or an online account.
- Dukovo gives apps no age or identity signal: no API, permission or
  manifest field says how old a user is or who they are. An app's
  manifest cannot ask for one.
- The Store shows user reviews and star ratings, but no age ratings, and
  nothing in it is held back by age.
- Publishers are known by their signing keys. Publishing to a catalog,
  or sideloading, never needs an ID document.
- Signing in to an outside service from an app, such as Mail's OAuth
  sign-in to a mail provider, is between the user and that service;
  Dukovo passes nothing about the user's age or identity to it.

Choices an owner makes about their own machine stay open to them. An
owner may limit what another account can install, and the approved
Settings design has optional Parental controls: time and app limits on
an account the owner chooses, off unless set up. These are the owner's
settings for an account, never an age: setting them up asks for no birth
date, and nothing tells an app about them.

## Consequences

- authd stores no birth date, and its account records have no field for
  one. The manifest schema (Phase 13d) has no age or identity field and
  no permission that gives one.
- Dukovo does not comply with laws that ask an operating system or app
  store for age checks or age signals, in California or anywhere else,
  whether or not an exemption covers it (the maintainer, 2026-10-27).
  California's Digital Age Assurance Act (AB 1043) applies from
  2027-01-01; AB 1856, passed by the legislature in August 2026, would
  exempt systems under licenses that let anyone copy, change and share
  them, as Apache-2.0 OR MIT does. The attorney check before Phase 14
  looks at what this exposes the project and its distributors to, so it
  can be planned for; it does not reopen the rule.
- Contributions that add any age or identity check are declined.
