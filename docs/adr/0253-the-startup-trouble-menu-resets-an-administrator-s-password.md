<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0253. The startup-trouble menu resets an administrator's password

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.6. With sign-in (ADR 0250) and elevate (ADR 0219), the only
way to act as root on a signed-in machine is an administrator's
password. The owner must never be locked out of their own machine, and
a server has no one else to ask. The security design's row for a lost
administrator's password, which Chase accepted on 2026-10-26, says:
"The startup-trouble menu at the console (the machine's own or a cloud
provider's) offers 'Reset an admin password', like Linux's single-user
mode; it is logged and shown at the next sign-in. Once disks are
encrypted (1.4) it needs the recovery key."

ADR 0250 already lets anyone at the boot menu start root's shell with
`console-sign-in=off`, as Linux's `init=/bin/sh`: the way back in for
those who know the kernel's command line. Ubuntu's answer for everyone
else is the recovery mode entry of its GRUB menu, whose menu has a root
shell where `passwd` resets a password.

## Decision

- **A second boot entry.** `ob image` writes a second GRUB menu entry,
  `Dukovo Server VERSION: startup trouble`, the first one's with the
  kernel flag `startup-trouble` as the command line's first word, so it
  is the kernel's whatever comes after a lone `--`. The kernel lists
  the flag and leaves it to login and authd (dukovo-boot(7)). The boot
  tests fill the command line into both entries.
- **The menu.** With the flag, login(1) shows the startup-trouble menu
  on each console before the sign-in: reset an administrator's
  password, sign in, restart, power off. A reset lists the
  administrators, asks whose, and asks the new password twice, warned
  about but never refused (no forced password rules). Restart and power
  off ask Spark, as reboot(8) does.
- **authd's `reset` request** sets an administrator's password: root's
  to ask, as login runs, and only while the kernel was started with
  `startup-trouble`, which authd reads from `/kernel/cmdline` itself.
  An account that is not an administrator's is refused, since an
  administrator resets it with `elevate passwd`. The wrong passwords
  given for the account are forgotten, so the new one works at once.
- **Nothing unseen.** A reset is a security event in the audit log, and
  authd keeps a notice for every administrator in
  `/system/state/authd/notices`. At each person's next sign-in authd
  writes theirs on the terminal before the session's programs start,
  and forgets them once the session started:
  `Your password was reset from the startup-trouble menu on ...` for
  them, `The password of alice was reset ...` for the others.

## Consequences

- The owner of a machine gets back in with three keys at its console,
  their own or a cloud provider's, and nothing to remember.
- Whoever stands at the boot menu could already start root's shell, so
  the menu gives no one more than before; what it adds is the record
  and the notices. A machine whose boot menu must be closed to its
  console's users needs a firmware and GRUB password, as Linux does.
- Once disks are encrypted (release 1.4), a reset will need the
  recovery key: without it, the password protects the data.
- The `startup-trouble` boot case makes the owner, resets her password
  from the menu, has the old one refused and the new one sign her in,
  checks the audit line and that she is told once, and powers off
  from the menu. signintest checks that root's `reset` is refused in a
  normal boot.
- **Not yet:** the graphical sign-in's version of the menu (Phase 14),
  and the notices for other events, such as a failed sign-in streak,
  which `last` (13b.7) can add.

## Alternatives considered

- **A root shell, as Ubuntu's recovery mode.** It is already there, as
  `console-sign-in=off`, for those who want it; a menu with one thing
  to do is clearer, and it records what was done.
- **Resetting any account's password.** Administrators reset anyone
  else's with `elevate passwd`; the menu is for getting an
  administrator back.
- **A reset that leaves no trace.** A person who finds their password
  changed should learn how and when, and so should the other
  administrators.
- **Gating the request on the terminal, not the kernel flag.** Root
  could send it from any terminal; the flag is what says the machine
  was started for startup trouble.
