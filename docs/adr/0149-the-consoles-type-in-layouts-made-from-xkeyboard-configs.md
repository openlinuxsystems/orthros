<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# 0149. The consoles type in keyboard layouts made from xkeyboard-config's

- Status: Accepted
- Date: 2026-10-28

## Context

consoled (ADR 0100) turns the key events of `/devices/kbd` into what a
terminal sends, and until now did so in the US layout only. Every
keyboard that is not American, which since ADR 0148 includes every USB
one, typed the wrong characters, and most of the world's letters could
not be typed at all.

A layout says what each key types alone, with Shift, with AltGr (Right
Alt on most non-US keyboards) and with both, and has *dead keys*: an
accent that goes on the next letter typed (`´` then `e` is `é`). Linux's
console reads its layouts from the kbd package's files with `loadkeys`;
those files are under the GPL, and their layouts often differ from the
ones the same machine has under X. X and Wayland take theirs from
xkeyboard-config, whose layouts are maintained for every country and
whose notices are MIT's and others like it, and systemd's
`localectl set-keymap` sets one setting for both. Spleen, the consoles'
font (ADR 0015), draws Latin-1, most of Latin Extended-A and the Cyrillic
the Russian layout needs.

## Decision

- The layouts are made from xkeyboard-config 2.48's by a script,
  `userspace/programs/keymap/generate.py`, which compiles each with
  libxkbcommon as X does for a pc105 keyboard and writes, for each key of
  the main block, its first four levels to a text file of our own
  format, keymap(5): a key's Linux name, then a character, `U+XXXX`,
  `-` for nothing, or `dead:ACCENT` for each level. 40 layouts are made:
  the US one and its international, Dvorak and Colemak variants, and
  those of the countries whose letters are Latin. The files are in the
  repository, with xkeyboard-config's COPYING as their notice, and built
  into a crate, `keymap`, which parses them (and a file of one's own),
  answers what a key types with Shift, AltGr and Caps Lock, and what a
  dead key makes of a letter.
- What a dead key makes of a letter is Unicode's: generate.py writes,
  for each of the 18 accents xkeyboard-config's Latin layouts use as dead
  keys, each letter Unicode composes with it into one character. A space
  after a dead key, or the dead key again, types the accent alone, and
  another character types the accent and then itself, as Linux's console
  does. The stroke, currency and Greek dead keys of a few layouts, which
  are not accents, type nothing.
- Right Alt is AltGr in a layout where xkeyboard-config makes it so, and
  Alt in the others. Caps Lock works as Shift on a key whose levels are a
  lowercase letter and its capital, as on Linux. Ctrl with a character
  that has no control character (`ö`) takes the control character of the
  key's US one, as X falls back to the first group, so Ctrl+C is Ctrl+C
  on every layout.
- The layout is a setting, `/system/settings/keyboard` (`layout = de`),
  `us` when there is none. consoled reads it when it starts and on SIGHUP.
  Spark gains a `reload` request, which sends a running service SIGHUP as
  systemd's ExecReload does, and service(8) a `reload` command, as
  Debian's has. A new command, keymap(1) in systools, prints the layout,
  lists them, and sets one: it saves the setting and asks Spark to reload
  consoled, so the consoles type in it at once.
- A file `/system/settings/keymaps/NAME.keymap` is a layout of that name
  too, in place of a built-in one: anyone can change a layout or make
  one without building Dukovo.

## Consequences

- The `consoles` boot test switches to the German layout with keymap(1),
  types a Z on the Y key, an é with a dead acute and an @ with AltGr,
  restarts consoled, which must keep the layout, and switches back. The
  `keymap` crate's host tests parse every layout, check the US one is
  Linux's, check the German, French, Dvorak, Colemak, Spanish, Polish,
  Turkish and Japanese layouts' keys, every dead key's letters, refused
  files, and that any text is safe to parse.
- Layouts whose letters are not Latin (Russian, Ukrainian, Greek,
  Hebrew, Arabic and others) need a second group and a key to switch to
  it, which xkeyboard-config has and the format does not yet; they come
  when one is asked for, and so does Compose. The keys beyond the main
  block (the keypad's other keys, the function and editing keys) keep
  the Linux console's meanings.
- The layout is the machine's, as Linux's console's is, until users
  (Phase 13) can each have their own; Settings (Phase 17) will set the
  same setting, and the desktop's layouts can come from the same files.
- A newer xkeyboard-config is taken by running generate.py on it and
  reading the diff.
