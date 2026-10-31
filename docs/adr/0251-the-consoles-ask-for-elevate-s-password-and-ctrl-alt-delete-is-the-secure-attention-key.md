<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0251. The consoles ask for elevate's password, and Ctrl+Alt+Delete is the secure attention key

- Status: Accepted
- Date: 2026-10-31

## Context

Phase 13b.6. With ADR 0219 elevate(8) asks for its person's password
itself, on the terminal, as sudo does. Any program the user runs can
draw the same words and read what is typed after them, so a program
left running in a pane, or a command named like elevate earlier in a
`PATH`, can take an administrator's password. The security design says
the password for elevate is typed into a prompt that no program draws
or reads, and the Phase 13 prep doc's question 2, which Chase accepted
on 2026-10-30, makes Ctrl+Alt+Delete the secure attention key: it
"always brings up the real sign-in or password prompt, which no program
ever receives", and restarting moves to `reboot` and the sign-in
screen's menu. Until now Ctrl+Alt+Delete on the consoles ran reboot(8),
as on Linux.

consoled already reads every key from the keyboard before any program
does, and draws the screen's bottom line itself for notifications (ADR
0212); no program can reach the keyboard or the screen while it runs.

## Decision

- **consoled asks for the passwords on its consoles.** It connects to
  authd with a new `prompts` request, root's to ask, and keeps the
  connection open. elevate sends its `elevate` and `unlock` requests
  with the new password kind `ask`. From a locked pane on a console,
  authd does not answer `ok password`: it keeps the request and its
  descriptors and writes `ask ID TERMINAL NAME COMMAND` to consoled.
  consoled shows the prompt on that console's bottom line, in black on
  cyan, as `[elevate] Password for alice, to run
  /system/commands/user add bob:`, and takes every key typed on that
  console while it shows: nothing echoes, and nothing reaches the
  console's programs. Enter sends the password to authd alone, in a
  `prompted ID password PASSWORD` request on a connection of its own,
  which only the process that asked `prompts` may send; authd takes it
  as the password the elevate request came with, with the usual wait
  after a wrong one, and elevate says `Sorry, try again.` and asks
  again. Esc or Ctrl+C close the prompt (`prompted ID cancelled`), and
  elevate says no password was typed. Backspace takes back a
  character, Ctrl+U all of them, and Alt+F1 to Alt+F6 still switch: the
  prompt waits on its console.
- **Anywhere else elevate asks.** A terminal that is not a console's,
  as over SSH (13c), is answered `prompted ID elsewhere`, and authd then
  answers `ok password`; so is every `ask` while consoled is not
  connected, or after it goes. `-S` and `-n` send `none` as before. A
  prompt not answered in 5 minutes, as sudo's `passwd_timeout`, is given
  up; when its elevate goes first, authd writes `done ID` and consoled
  takes the prompt off.
- **Ctrl+Alt+Delete is the secure attention key.** It never reaches a
  program, as before. When the shown console has a password prompt, it
  clears what was typed there, so whatever is typed next goes to
  consoled. Otherwise it shows consoled's own menu on the bottom line:
  Enter shows the sign-in, on the shown console if nobody is signed in
  there, or else on the first console where nobody is, where only
  consoled's login(1) runs on a new terminal; R runs reboot(8); P runs
  poweroff(8); Esc closes it. With root's shell on the consoles
  (`console-sign-in=off`) the menu has no Enter and consoled asks for no
  passwords. Whether someone is signed in is whether a session has the
  console's terminal as its controlling terminal (`tcgetsid` on the
  master), which login does not take (ADR 0250).

## Consequences

- On the consoles an administrator's password goes from the keyboard to
  consoled and authd only. A program can still draw a line that looks
  like the prompt, but not one that stays through Ctrl+Alt+Delete; the
  handbook says to press it when a prompt looks wrong.
- Ctrl+Alt+Delete no longer restarts at once: it takes R after it, as a
  graphical sign-in screen's power menu does. Holding the power button,
  or `reboot`, still restart.
- authd keeps one more kind of waiting request, with its descriptors,
  for at most 5 minutes; consoled holds one more connection, to authd,
  and links the accounts crate for the requests.
- Prompts show only on the console they belong to; a person who
  switches away finds theirs waiting when they come back.
- `-p` sets elevate's own prompt only; consoled's keeps its words.
- The `elevate` boot case types the password at consoled's prompt,
  clears a first try with Ctrl+Alt+Delete and finds none of it in the
  shell, closes a prompt with Esc, reads one with `-S`, and opens the
  menu, closes it, and signs in on tty3 through it.
- **Not yet:** the graphical sign-in and the desktop's elevate prompt
  (Phase 14), which follow the same pattern with the compositor in
  consoled's place; SSH's own prompt (13c).

## Alternatives considered

- **Linux's SAK, which kills every process on the terminal.** It ends
  the person's own work along with a fake prompt, and Linux leaves it
  off by default for that reason. Showing the system's own prompt or
  menu instead keeps the session.
- **elevate asks consoled for the prompt and gets the password back.**
  Any program could then have consoled show a real prompt and read
  what is typed into it; the password has to go to authd alone.
- **A separate prompt service.** consoled already owns the keyboard and
  the screen, and a second program would need both handed to it; the
  desktop's compositor will be the same single owner.
- **Keeping Ctrl+Alt+Delete as an immediate restart.** A key that both
  guards passwords and restarts the machine would restart it whenever
  someone checks a prompt.
