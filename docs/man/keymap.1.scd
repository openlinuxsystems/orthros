keymap(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

keymap - the keyboard layout the consoles type in

# SYNOPSIS

*keymap* [*--list* | _name_]

# DESCRIPTION

With no argument, keymap prints the name of the layout the consoles type
in, and what it is called:
```
de              German
```
With a _name_, it saves that layout in _/system/settings/keyboard_ and
asks Spark to have consoled read it again (*service consoled reload*),
so the consoles type in it at once, and after every restart:
```
The consoles type in German (de) now.
```
Every keyboard types in the same layout, the PS/2 one and every USB one.

Dukovo has 40 layouts, made from xkeyboard-config's, which X and Wayland
use: *us* (the default), *us-intl*, *us-altgr-intl*, *us-dvorak*,
*us-colemak*, *gb*, *ie*, *de*, *at*, *ch*, *ch-fr*, *fr*, *be*, *nl*,
*es*, *latam*, *pt*, *br*, *it*, *dk*, *no*, *se*, *fi*, *is*, *fo*, *ee*,
*lv*, *lt*, *pl*, *cz*, *sk*, *hu*, *si*, *hr*, *ro*, *tr*, *mt*, *al*,
*ca* and *jp*: each X's layout of that name, or, after a *-*, that
layout's variant. Right Alt is AltGr in a layout that has a third level,
and its dead keys put an accent on the letter typed after them, as
*keymap*(5) describes. A file of one's own,
_/system/settings/keymaps/name.keymap_, is a layout of that name too,
used in place of a built-in one of the same name.

Linux's *localectl set-keymap* does as much for its consoles; Linux's own
console layouts are the kbd package's, not X's.

# OPTIONS

*-l*, *--list*
	Every layout: its name and what it is called, the built-in ones
	first, then one's own, marked *(own)*.

# EXIT STATUS

0 when it printed or saved the layout; 1 for a layout there is not, or
a setting it cannot read or write; 2 for an option it does not know.

# FILES

_/system/settings/keyboard_
	The setting: a line *layout =* _name_. With no setting, the
	consoles type in *us*.

_/system/settings/keymaps/name.keymap_
	One's own layouts (*keymap*(5)).

# SEE ALSO

*keymap*(5), *consoled*(8), *service*(8), *systools*(1)
