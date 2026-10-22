<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub's private
vulnerability reporting: open the repository's
[Security tab](https://github.com/openlinuxsystems/orthros/security) and
choose **Report a vulnerability**, or go straight to
<https://github.com/openlinuxsystems/orthros/security/advisories/new>.
Only the maintainers ([GOVERNANCE.md](GOVERNANCE.md)) can see the report.

Do not open a public issue, pull request or discussion about a
vulnerability before it is fixed and disclosed.

A useful report says which commit or release is affected, what an attacker
can do, and how to reproduce it: an `./ob run --cmdline` line, a boot test,
or a program, if you have one.

## What happens next

| When | What we do |
| --- | --- |
| Within 3 working days | Acknowledge the report. |
| Within 14 days | Confirm whether it is a vulnerability, tell you how severe we think it is, and agree on a disclosure date with you. |
| Within 90 days of the report | Release a fix and publish a GitHub security advisory, requesting a CVE where one applies. |

If a fix needs longer, for example because it depends on a third party, we
tell you why and agree on a new date; we do not keep a report secret
indefinitely. If the vulnerability is being exploited or is already public,
we fix and disclose it as fast as we can. If 90 days pass without a fix or
an agreed extension, you are free to disclose it yourself.

We credit you in the advisory unless you ask us not to. We do not run a
bug bounty.

## Supported versions

Dukovo has not had a release yet; only the `main` branch is supported, and
fixes land there. Once releases begin, this section will list which ones
receive security fixes.

## Scope

In scope: everything in this repository, including the Radix kernel, the
`ob` build tool and the images it builds. A vulnerability in a third-party
component that Dukovo ships, such as GRUB, is also welcome; we will pass it
on to that project and update the component.
