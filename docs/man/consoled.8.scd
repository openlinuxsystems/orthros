consoled(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

consoled - the virtual consoles on the screen and keyboard

# SYNOPSIS

*consoled* [*--consoles* _n_] [*--shell* _path_]

# DESCRIPTION

consoled draws Dukovo's virtual consoles, *tty1* to *tty6*, on the
screen, one at a time, and types what is pressed on the keyboard into
the one shown. Each console is a terminal for *TERM=linux*, a VT100 with
the Linux console's colours, keys and escape sequences, running the
sign-in, *login*(1), on a pseudo-terminal of its own (*radix-posix*(7)).

The Radix kernel shows its log on the screen while the machine starts.
When consoled takes the screen (_/devices/fb0_, *radix-framebuffer*(2)) the
kernel stops drawing on it, and when consoled ends the screen is the
log's again; *dmesg*(1) shows the kernel's messages meanwhile. The keys
come from _/devices/kbd_ (*radix-input*(2)): the PS/2 keyboard's events
and every USB keyboard's, which can be plugged in and pulled out at any
time.

Spark starts consoled at boot as the *consoled* service
(_/system/settings/spark/services/consoled.toml_, *spark*(8)), first and
stopped last, and starts it again if it fails. Its file asks for
*admin.hardware*, so that root's shells may set the fans, the LEDs and
the ARGB header with *ectool*(8), and for the whole network with
*admin.network*, so that they may connect, listen on any port and set
the interfaces up with *ip*(8). Without a screen, as on
a machine with only a serial console, consoled logs *no screen for
consoles* and exits with 0, and stays stopped.

# SESSIONS

tty1's sign-in starts with consoled; each other console's starts the
first time it is shown. consoled runs _/system/commands/login_ as root
on the console's terminal, in a session of its own, with *TERM=linux*
and the screen's size on the terminal. Before it, the console shows
_/system/settings/issue_, in which *\\s* is the system's name, *\\r* its
release, *\\l* the console's name, *\\n* the host's name, *\\v* the
kernel's version and *\\m* the machine:

```
Dukovo Server 0.11.0 (tty1)

Name:
```

login asks for a name and a password, and *authd*(8) starts the
person's session on that terminal, acting for them; on a machine with
no account yet it first makes the owner's (*login*(1), ADR 0250). Each
console signs in on its own, so two people, or one person twice, can be
signed in on two consoles at once.

Above the sign-in prompt, in issue's place, each console shows the
welcome banner: Duke and the name in plain ASCII, the tagline, and the
system's name and version (ADR 0261). *welcome.toml*(5) says when it
shows and what is in it; a screen too small for Duke gets less. Consoles
that start root's shell with no sign-in show issue's text.

When the sign-in ends, because the session it started ended, as with
*exit*, or because its input did, the console clears and the sign-in
starts again. One that ends within 2 seconds five times in a row waits
10 seconds before the next.

With *--shell*, or with *console-sign-in=off* on the kernel's command
line (*dukovo-boot*(7)), there is no sign-in: each console runs root's
login shell from _/system/settings/passwd_ (*nukesh*(1) by default), as
*-nukesh*, with *HOME*, *SHELL*, *USER*, *LOGNAME* and *TERM=linux* set,
and with no password asked, and consoled logs *console-sign-in=off:
root's shell on each console, no sign-in*. Only someone who can already
change the kernel's command line, at the boot menu or in the boot
volume, can turn it off; the boot tests do, to type at root's shell.

# NOTIFICATIONS

consoled shows the notifications *notifyd*(8) passes on, such as
*notify-send*(1)'s, on the bottom line of the screen, over the shown
console's last line: the name of the program that sent it, its title
and its body on one line, cut with *...* if it is too long, in white on
blue, or in red for a critical one and grey for a low one (ADR 0212). It
stays for its time, 8 seconds unless it asked for another; a critical
one, or one of 0 seconds, stays until a key is pressed, and the key
still goes to the console. A newer notification takes the line at once,
and *notifications*(1) lists them all. consoled logs *showing
notifications from notifyd* when it finds notifyd, and tries again every
5 seconds while it is not running.

# PASSWORDS

When *elevate*(8), or *sudo*(8), needs its person's password in a pane
on one of the consoles, consoled asks for it, not elevate (ADR 0251).
*authd*(8) tells consoled, which shows the prompt on that console's
bottom line, in black on cyan:

```
 [elevate] Password for alice, to run /system/commands/user add bob:
```

The keys typed while it shows are consoled's: what is typed never
shows and never reaches the console's programs, *Enter* sends it to
authd alone, on a connection of its own, *Backspace* takes back a
character and *Ctrl+U* all of them, and *Esc* or *Ctrl+C* close the
prompt, and elevate says no password was typed. Alt+F1 to Alt+F6 still
show the other consoles; the prompt waits on its own. No program can
read the password, and none can draw this prompt so that it takes
keys: *Ctrl+Alt+Delete*, which no program receives, brings the real one
back cleared, or shows consoled's menu if there is none. A wrong
password is asked again by a new prompt, after authd's wait, and
elevate says *Sorry, try again.* on the terminal.

*bundled*(8)'s questions about apps' permissions come the same way
(ADR 0224), on the bottom line, in black on cyan, or white on red for
one with a warning:

```
 [permission] Notes (org.example.notes) asks to: use your documents folder, to read and change it      O: once    A: always    D: don't allow
```

*O* allows it this time, *A* from now on, *D* never, and *Esc* or
*Ctrl+C* close it; *bundle grant*'s question takes *Y* or *N*. No other
key does anything while it shows, and none reaches the console's
programs. On a console narrower than the question, its words are cut
with *...*, never its keys.

consoled logs *asking for passwords on the consoles for authd* when it
finds authd, and tries again every 5 seconds while it is not running;
meanwhile, and on any other terminal, as over SSH, elevate asks itself.
With root's shell on each console and no sign-in, consoled asks for no
passwords.

# KEYS

*Alt+F1* to *Alt+F6*
	Show that console. *Alt+Left* and *Alt+Right* go to the program on
	the console, as in a terminal window: *nukesh*(1) goes back and
	forward through its folders with them.

*Shift+Page Up*, *Shift+Page Down*
	Look back through the console's last 1000 lines and forward again.
	Any key typed goes back to the end.

*Scroll Lock*
	Hold the shown console: what its programs write waits until Scroll
	Lock is pressed again.

*Ctrl+Alt+Delete*
	The secure attention key, which no program ever receives (ADR
	0251). When the shown console asks for a password, it clears what
	was typed there, so that what is typed next goes to consoled alone.
	Otherwise it shows consoled's menu on the bottom line: *Enter*
	shows the sign-in, on the shown console if nobody is signed in
	there, or else on the first console where nobody is; *R* runs
	*reboot*(8) and *P* *poweroff*(8); *Esc* closes it. With root's
	shell on each console and no sign-in, the menu has no *Enter*.

*Caps Lock*, *Num Lock*
	As on any keyboard; every keyboard's lights follow them. The keypad
	types digits while Num Lock is on, which it is from the start.

The keys type in the layout _/system/settings/keyboard_ names, US
English unless it names another (*keymap*(1)), which every keyboard
shares: consoled reads it when it starts and when it is sent SIGHUP,
which *service consoled reload* does. In a layout that has them, Right
Alt is AltGr, and a dead key puts its accent on the letter typed after
it (*keymap*(5)). Ctrl with a key whose character has no control
character sends the control character of the key's US one. The other
keys send what the Linux console sends: *F1* to *F5* as *ESC [ [ A* to *ESC [ [ E*, *F6* to *F12*
as *ESC [ 17 ~* to *ESC [ 24 ~*, *Home* to *Page Down* as *ESC [ 1 ~* to
*ESC [ 6 ~*, the arrows as *ESC [ A* to *ESC [ D* (*ESC O A* in
application cursor mode); *Alt* with a key sends *ESC* before it.

# TERMINAL

The consoles understand what the Linux console's terminfo entry
(*linux*) lists: cursor movement, erasing, inserting and deleting
characters and lines, scrolling regions, tab stops, the alternate
character set for line drawing, saving and restoring the cursor,
insert, origin, autowrap and reverse-video modes, application cursor
and keypad modes, and SGR's bold, dim, underline, reverse, the 16
colours, the 256 colours and true colour, the latter two shown as the
nearest of the 256. Wide characters take two cells. The parser is the
vte crate's, from the Alacritty project.

# OPTIONS

*--consoles* _n_
	Have _n_ consoles, 1 to 12, rather than 6.

*--shell* _path_
	Start _path_ as root's login shell in each session, with no
	sign-in.

*--help*, *--version*
	Show how to call it, or its version.

# FILES

_/devices/fb0_
	The screen.

_/devices/kbd_
	The keyboards' events.

_/system/settings/issue_
	What each console shows before its session. Its default is in
	_/system/resources/default-settings/issue_.

_/system/settings/console/welcome.toml_
	The welcome banner (*welcome.toml*(5)).

_/system/settings/keyboard_
	The keyboard layout's setting (*keymap*(1)).

_/system/settings/keymaps/name.keymap_
	One's own layouts (*keymap*(5)).

_/system/settings/spark/services/consoled.toml_
	The service.

_/system/live/notify/socket_
	notifyd's socket, which consoled follows.

_/system/live/authd/socket_
	authd's socket, which consoled asks for prompts and answers them on.

# SEE ALSO

*login*(1), *authd*(8), *elevate*(8), *spark*(8), *reboot*(8),
*keymap*(1), *keymap*(5), *welcome.toml*(5), *nukesh*(1), *dmesg*(1),
*notifyd*(8), *notify-send*(1), *dukovo-boot*(7),
*radix-framebuffer*(2), *radix-input*(2), *radix-files*(7)
