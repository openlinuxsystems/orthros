keymap(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

keymap - a keyboard layout for the consoles

# DESCRIPTION

A keymap file says what each key a console types with types: alone,
with Shift, with AltGr, and with Shift and AltGr. Dukovo's own layouts
are built into consoled and *keymap*(1); a file of one's own goes in
_/system/settings/keymaps/name.keymap_ (*keymap*(1)). A file is UTF-8
text of at most 64 KiB; a line starting with *#* is a comment, and blank
lines are skipped.

*name =* _text_
	What the layout is called: 1 to 64 characters. Every file has
	one.

*rightalt =* *altgr* | *alt*
	Whether Right Alt is AltGr, which picks the third and fourth
	levels, or Alt, as the left one is (the default).

_key_ _level_ ...
	A key, by its Linux name, and one to four levels: alone, with
	Shift, with AltGr, with Shift and AltGr. A key a file does not
	name types nothing. The keys are the main block's: *1* to *0*,
	*minus*, *equal*, *q* to *p*, *leftbrace*, *rightbrace*, *a* to
	*l*, *semicolon*, *apostrophe*, *grave*, *backslash*, *z* to *m*,
	*comma*, *dot*, *slash*, *space*, *102nd* (the key next to the left
	Shift on a European keyboard), *ro* and *yen* (on a Japanese or a
	Brazilian one), and *kpdot*, whose one level is what the keypad's
	decimal key types with Num Lock on. Each name is that of
	_KEY\_NAME_ in *radix-input*(2), lowercase, without _KEY\__.

A level is:

_c_
	The character _c_.

*U+XXXX*
	The character of that number, in 4 to 6 hexadecimal digits: for
	*-*, a space, or a combining mark.

*-*
	Nothing.

*dead:*_accent_
	A dead key: it types nothing, and puts its accent on the letter
	typed next, if Unicode has that letter with that accent; a space
	after it, or the dead key again, types the accent alone, and any
	other character types the accent and then itself. The accents
	are *grave*, *acute*, *circumflex*, *tilde*, *macron*, *breve*,
	*abovedot*, *diaeresis*, *hook*, *abovering*, *doubleacute*,
	*caron*, *horn*, *belowdot*, *belowcomma*, *cedilla*, *ogonek* and
	*belowmacron*, xkeyboard-config's names after _dead\__.

A level a line does not give is the one without AltGr, and then the
first: a space bar given one level types a space with Shift and AltGr
too. Caps Lock works as Shift on a key whose two levels are a lowercase
letter and its capital, and Ctrl with a key whose character has no
control character types the control character of the key's US one.

# EXAMPLE

The start of the German layout:
```
name = German
rightalt = altgr

1           1        !        ¹        ¡
minus       ß        ?        \\        ¿
equal       dead:acute dead:grave dead:cedilla dead:ogonek
q           q        Q        @        Ω
y           z        Z        ←        ¥
```

# SEE ALSO

*keymap*(1), *consoled*(8), *radix-input*(2)
