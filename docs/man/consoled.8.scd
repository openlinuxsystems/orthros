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
the Linux console's colours, keys and escape sequences, running a login
shell on a pseudo-terminal of its own (*radix-posix*(7)).

The Radix kernel shows its log on the screen while the machine starts.
When consoled takes the screen (_/devices/fb0_, *radix-framebuffer*(2)) the
kernel stops drawing on it, and when consoled ends the screen is the
log's again; *dmesg*(1) shows the kernel's messages meanwhile. The keys
come from _/devices/kbd_ (*radix-input*(2)): the PS/2 keyboard's events
and every USB keyboard's, which can be plugged in and pulled out at any
time.

Spark starts consoled at boot as the *consoled* service
(_/system/settings/spark/services/consoled.service_, *spark*(8)), first and
stopped last, and starts it again if it fails. Its file asks for
*admin.hardware*, so that root's shells may set the fans, the LEDs and
the ARGB header with *ectool*(8). Without a screen, as on
a machine with only a serial console, consoled logs *no screen for
consoles* and exits with 0, and stays stopped.

# SESSIONS

tty1's session starts with consoled; each other console's starts the
first time it is shown. A session is root's login shell, from
_/system/settings/passwd_ (*nukesh*(1) by default), started as *-nukesh*
with *HOME*, *SHELL*, *USER*, *LOGNAME* and *TERM=linux* set and the
screen's size on its terminal. Before it, the console shows
_/system/settings/issue_, in which *\\s* is the system's name, *\\r* its
release, *\\l* the console's name, *\\n* the host's name, *\\v* the
kernel's version and *\\m* the machine:

```
Dukovo Server 0.10.4 (tty1)

#
```

When a session ends, as with *exit*, the console clears and a new one
starts. A session that ends within 2 seconds five times in a row waits
10 seconds before the next. Users and login(1) come in Phase 13 of the
roadmap; until then each session is root's, with no password asked.

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

# KEYS

*Alt+F1* to *Alt+F6*
	Show that console. *Alt+Left* and *Alt+Right* show the one before
	or after.

*Shift+Page Up*, *Shift+Page Down*
	Look back through the console's last 1000 lines and forward again.
	Any key typed goes back to the end.

*Scroll Lock*
	Hold the shown console: what its programs write waits until Scroll
	Lock is pressed again.

*Ctrl+Alt+Delete*
	Run *reboot*(8).

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
	Start _path_ in each session rather than root's shell.

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

_/system/settings/keyboard_
	The keyboard layout's setting (*keymap*(1)).

_/system/settings/keymaps/name.keymap_
	One's own layouts (*keymap*(5)).

_/system/settings/spark/services/consoled.service_
	The service.

_/system/live/notify/socket_
	notifyd's socket, which consoled follows.

# SEE ALSO

*spark*(8), *reboot*(8), *keymap*(1), *keymap*(5), *nukesh*(1), *dmesg*(1),
*notifyd*(8), *notify-send*(1),
*radix-framebuffer*(2), *radix-input*(2), *radix-files*(7)
