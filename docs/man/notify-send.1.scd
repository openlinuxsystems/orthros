notify-send(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

notify-send - send a notification

# SYNOPSIS

*notify-send* [_option_...] _title_ [_body_]

# DESCRIPTION

notify-send sends a notification to *notifyd*(8), which passes it to
the screen: the console shows it on its bottom line, after the name of
the program that sent it, for 8 seconds or the time asked for (ADR
0212). The options are libnotify's, so that scripts written for Linux
work.

```
notify-send -a backup 'Backup done' 'All 3 disks are copied.'
```

shows

```
 notify-send (backup): Backup done - All 3 disks are copied.
```

The title is one line; a body may have several, which the console
shows joined on its one line and *notifications*(1) shows in full.

# OPTIONS

*-u*, *--urgency* _level_
	*low*, *normal* (the default) or *critical*. A critical
	notification stays until a key is pressed, in red; a low one is
	shown in grey.

*-t*, *--expire-time* _ms_
	Show it this many milliseconds, rounded up to seconds, at most 60;
	0 shows it until a key is pressed.

*-a*, *--app-name* _name_
	The name the program goes by, shown after the name of the program
	that sent it.

*-p*, *--print-id*
	Print the notification's number.

*-i*, *--icon* _icon_, *-c*, *--category* _type_, *-h*, *--hint* _hint_, *-e*, *--transient*
	Taken and ignored: the console shows no icons, and keeps no
	notification.

# EXIT STATUS

0 when notifyd took the notification; 1 when it refused it, as for a
flood of more than 10 in 10 seconds, when notifyd is not running, or
for a mistake in the command line.

# SEE ALSO

*notifications*(1), *notifyd*(8), *consoled*(8)
