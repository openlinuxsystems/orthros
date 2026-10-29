#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT

"""Makes the consoles' keyboard layouts (ADR 0149) from xkeyboard-config.

    python3 userspace/programs/keymap/generate.py XKEYBOARD_CONFIG_DIR

XKEYBOARD_CONFIG_DIR is an unpacked xkeyboard-config release, whose
version it reads from meson.build: the layouts in the repository are
2.48's, from Ubuntu's mirror of the upstream archive,
https://archive.ubuntu.com/ubuntu/pool/main/x/xkeyboard-config/xkeyboard-config_2.48.orig.tar.xz
(SHA-256 b77041324f0109f77161ee43743fe04baa485866af8460d31e476ad3f7648fd5).
libxkbcommon compiles each layout as X does for a pc105 keyboard, and
this writes, for the keys a console types with, the characters and dead
keys of its four levels to keymaps/NAME.keymap, the list of layouts to
src/layouts.rs, and what each dead key makes of each letter to
src/dead.rs, from Unicode's canonical compositions (Python's
unicodedata). It needs Python 3 and libxkbcommon (libxkbcommon0 on
Debian, Ubuntu and Linux Mint).
"""

import ctypes
import os
import re
import subprocess
import sys
import unicodedata

# What each keymap file is called, the xkeyboard-config layout and
# variant it is made from, in the order `keymap --list` shows them.
LAYOUTS = [
    ("us", "us", ""),
    ("us-intl", "us", "intl"),
    ("us-altgr-intl", "us", "altgr-intl"),
    ("us-dvorak", "us", "dvorak"),
    ("us-colemak", "us", "colemak"),
    ("gb", "gb", ""),
    ("ie", "ie", ""),
    ("de", "de", ""),
    ("at", "at", ""),
    ("ch", "ch", ""),
    ("ch-fr", "ch", "fr"),
    ("fr", "fr", ""),
    ("be", "be", ""),
    ("nl", "nl", ""),
    ("es", "es", ""),
    ("latam", "latam", ""),
    ("pt", "pt", ""),
    ("br", "br", ""),
    ("it", "it", ""),
    ("dk", "dk", ""),
    ("no", "no", ""),
    ("se", "se", ""),
    ("fi", "fi", ""),
    ("is", "is", ""),
    ("fo", "fo", ""),
    ("ee", "ee", ""),
    ("lv", "lv", ""),
    ("lt", "lt", ""),
    ("pl", "pl", ""),
    ("cz", "cz", ""),
    ("sk", "sk", ""),
    ("hu", "hu", ""),
    ("si", "si", ""),
    ("hr", "hr", ""),
    ("ro", "ro", ""),
    ("tr", "tr", ""),
    ("mt", "mt", ""),
    ("al", "al", ""),
    ("ca", "ca", ""),
    ("jp", "jp", ""),
]

# The keys a console types with, by their Linux names (radix-input(2)),
# with their codes; evdev's keycodes are these plus 8.
KEYS = [
    ("1", 2), ("2", 3), ("3", 4), ("4", 5), ("5", 6), ("6", 7), ("7", 8),
    ("8", 9), ("9", 10), ("0", 11), ("minus", 12), ("equal", 13),
    ("q", 16), ("w", 17), ("e", 18), ("r", 19), ("t", 20), ("y", 21),
    ("u", 22), ("i", 23), ("o", 24), ("p", 25), ("leftbrace", 26),
    ("rightbrace", 27), ("a", 30), ("s", 31), ("d", 32), ("f", 33),
    ("g", 34), ("h", 35), ("j", 36), ("k", 37), ("l", 38),
    ("semicolon", 39), ("apostrophe", 40), ("grave", 41),
    ("backslash", 43), ("z", 44), ("x", 45), ("c", 46), ("v", 47),
    ("b", 48), ("n", 49), ("m", 50), ("comma", 51), ("dot", 52),
    ("slash", 53), ("space", 57), ("102nd", 86), ("ro", 89), ("yen", 124),
    ("kpdot", 83),
]

# The dead keys the consoles have: each keysym's name after `dead_`, the
# combining mark it puts on the next letter, and what it types on its own
# (after a space, or pressed twice).
DEAD = [
    ("grave", "̀", "`"),
    ("acute", "́", "´"),
    ("circumflex", "̂", "^"),
    ("tilde", "̃", "~"),
    ("macron", "̄", "¯"),
    ("breve", "̆", "˘"),
    ("abovedot", "̇", "˙"),
    ("diaeresis", "̈", "¨"),
    ("hook", "̉", "̉"),
    ("abovering", "̊", "˚"),
    ("doubleacute", "̋", "˝"),
    ("caron", "̌", "ˇ"),
    ("horn", "̛", "̛"),
    ("belowdot", "̣", "̣"),
    ("belowcomma", "̦", ","),
    ("cedilla", "̧", "¸"),
    ("ogonek", "̨", "˛"),
    ("belowmacron", "̱", "_"),
]
DEAD_NAMES = {name for name, _, _ in DEAD}

HERE = os.path.dirname(os.path.abspath(__file__))


class Xkb:
    """libxkbcommon, through ctypes."""

    def __init__(self, tree):
        lib = ctypes.CDLL("libxkbcommon.so.0")
        lib.xkb_context_new.restype = ctypes.c_void_p
        lib.xkb_context_include_path_append.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        lib.xkb_keymap_new_from_string.restype = ctypes.c_void_p
        lib.xkb_keymap_new_from_string.argtypes = [
            ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        lib.xkb_keymap_unref.argtypes = [ctypes.c_void_p]
        lib.xkb_keymap_layout_get_name.restype = ctypes.c_char_p
        lib.xkb_keymap_layout_get_name.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        lib.xkb_keymap_num_levels_for_key.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32]
        lib.xkb_keymap_key_get_syms_by_level.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
            ctypes.POINTER(ctypes.POINTER(ctypes.c_uint32))]
        lib.xkb_keysym_to_utf32.restype = ctypes.c_uint32
        lib.xkb_keysym_to_utf32.argtypes = [ctypes.c_uint32]
        lib.xkb_keysym_get_name.argtypes = [ctypes.c_uint32, ctypes.c_char_p, ctypes.c_size_t]
        self.lib = lib
        # XKB_CONTEXT_NO_DEFAULT_INCLUDES: only the tree given.
        self.ctx = lib.xkb_context_new(1)
        if not self.ctx or not lib.xkb_context_include_path_append(self.ctx, tree.encode()):
            sys.exit(f"generate.py: libxkbcommon cannot read {tree}")

    def keymap(self, layout, variant):
        """The keymap X makes of `layout(variant)` on a pc105 keyboard."""
        symbols = f"{layout}({variant})" if variant else layout
        source = (
            "xkb_keymap {\n"
            '  xkb_keycodes { include "evdev+aliases(qwerty)" };\n'
            '  xkb_types { include "complete" };\n'
            '  xkb_compat { include "complete" };\n'
            f'  xkb_symbols {{ include "pc+{symbols}+inet(evdev)" }};\n'
            "};\n"
        )
        # XKB_KEYMAP_FORMAT_TEXT_V1.
        keymap = self.lib.xkb_keymap_new_from_string(self.ctx, source.encode(), 1, 0)
        if not keymap:
            sys.exit(f"generate.py: libxkbcommon cannot compile {symbols}")
        return keymap

    def name(self, keymap):
        return self.lib.xkb_keymap_layout_get_name(keymap, 0).decode()

    def keysym_name(self, keymap, code):
        """The name of the keysym of a key's first level."""
        syms = ctypes.POINTER(ctypes.c_uint32)()
        if self.lib.xkb_keymap_key_get_syms_by_level(keymap, code + 8, 0, 0, ctypes.byref(syms)) < 1:
            return None
        buf = ctypes.create_string_buffer(64)
        self.lib.xkb_keysym_get_name(syms[0], buf, 64)
        return buf.value.decode()

    def levels(self, keymap, code):
        """The first four levels of a key, each a character, a dead key's
        name (`dead:acute`) or None."""
        out = []
        count = self.lib.xkb_keymap_num_levels_for_key(keymap, code + 8, 0)
        for level in range(4):
            syms = ctypes.POINTER(ctypes.c_uint32)()
            n = 0
            if level < count:
                n = self.lib.xkb_keymap_key_get_syms_by_level(
                    keymap, code + 8, 0, level, ctypes.byref(syms))
            if n < 1:
                out.append(None)
                continue
            keysym = syms[0]
            utf32 = self.lib.xkb_keysym_to_utf32(keysym)
            if utf32:
                out.append(chr(utf32))
                continue
            buf = ctypes.create_string_buffer(64)
            self.lib.xkb_keysym_get_name(keysym, buf, 64)
            name = buf.value.decode()
            dead = name.removeprefix("dead_")
            out.append(f"dead:{dead}" if name.startswith("dead_") and dead in DEAD_NAMES else None)
        return out


def token(sym):
    """A level as a keymap file writes it."""
    if sym is None:
        return "-"
    if sym.startswith("dead:"):
        return sym
    c = sym
    if c == "-" or not c.isprintable() or c.isspace() or unicodedata.combining(c):
        return f"U+{ord(c):04X}"
    return c


def version(tree):
    meson = os.path.join(tree, "meson.build")
    text = open(meson, encoding="utf-8").read()
    found = re.search(r"version:\s*'([0-9.]+)'", text)
    if not found:
        sys.exit(f"generate.py: no version in {meson}")
    return found.group(1)


HEADER = """\
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-FileCopyrightText: The xkeyboard-config contributors
# SPDX-License-Identifier: MIT
#
# {name}: {description}, made by generate.py from xkeyboard-config
# {version}'s `{symbols}` layout (keymaps/COPYING-xkeyboard-config).
# keymap(5) describes the format: a key's Linux name, then what it types
# alone, with Shift, with AltGr and with Shift and AltGr.

name = {description}
{altgr}"""


def write_keymap(xkb, tree_version, file, layout, variant):
    keymap = xkb.keymap(layout, variant)
    description = xkb.name(keymap)
    symbols = f"{layout}({variant})" if variant else layout
    # Right Alt is AltGr, which picks the third and fourth levels, where
    # the layout makes it so; elsewhere it is Alt.
    altgr = "rightalt = altgr\n" if xkb.keysym_name(keymap, 100) == "ISO_Level3_Shift" else ""
    lines = [HEADER.format(name=file, description=description, version=tree_version,
                           symbols=symbols, altgr=altgr)]
    width = max(len(name) for name, _ in KEYS) + 2
    for name, code in KEYS:
        levels = xkb.levels(keymap, code)
        if name == "kpdot":
            # Only what it types with Num Lock on matters.
            levels = [levels[1], None, None, None]
        while levels and levels[-1] is None:
            levels.pop()
        if not levels:
            continue
        lines.append(name.ljust(width) + " ".join(token(sym).ljust(8) for sym in levels).rstrip())
    xkb.lib.xkb_keymap_unref(keymap)
    path = os.path.join(HERE, "keymaps", f"{file}.keymap")
    with open(path, "w", encoding="utf-8") as out:
        out.write("\n".join(lines) + "\n")
    return description


def write_layouts(described):
    rows = "\n".join(
        f'    ("{file}", include_str!("../keymaps/{file}.keymap")),' for file, _ in described)
    text = f"""\
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The layouts Dukovo has, made by generate.py: each name, and its file.

/// Every layout, by name, in the order `keymap --list` shows them.
pub const LAYOUTS: &[(&str, &str)] = &[
{rows}
];
"""
    with open(os.path.join(HERE, "src", "layouts.rs"), "w", encoding="utf-8") as out:
        out.write(text)


def variant_of(name):
    """A dead key's name in Rust: `abovering` is `AboveRing`."""
    for prefix in ("above", "below", "double"):
        if name.startswith(prefix):
            return prefix.capitalize() + name[len(prefix):].capitalize()
    return name.capitalize()


def write_dead():
    variants, names, spacings, tables, arms = [], [], [], [], []
    bases = [chr(c) for c in range(ord("A"), ord("Z") + 1)] + \
        [chr(c) for c in range(ord("a"), ord("z") + 1)]
    for name, mark, spacing in DEAD:
        variant = variant_of(name)
        variants.append(f"    /// `dead:{name}`, U+{ord(mark):04X}.\n    {variant},")
        names.append(f'            Dead::{variant} => "{name}",')
        spacings.append(f"            Dead::{variant} => '\\u{{{ord(spacing):x}}}',")
        pairs = []
        for base in bases:
            composed = unicodedata.normalize("NFC", base + mark)
            if len(composed) == 1:
                pairs.append(f"('{base}', '\\u{{{ord(composed):x}}}')")
        const = name.upper()
        tables.append(f"const {const}: &[(char, char)] = &[{', '.join(pairs)}];")
        arms.append(f"            Dead::{variant} => {const},")
    all_list = ", ".join(f"Dead::{variant_of(name)}" for name, _, _ in DEAD)
    text = f"""\
// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The dead keys, made by generate.py from Unicode {unicodedata.unidata_version}'s
//! canonical compositions: what each makes of each letter.

/// A dead key: an accent put on the next letter typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dead {{
{chr(10).join(variants)}
}}

impl Dead {{
    /// Every dead key.
    pub const ALL: [Dead; {len(DEAD)}] = [{all_list}];

    /// Its name in a keymap file, after `dead:`.
    pub fn name(self) -> &'static str {{
        match self {{
{chr(10).join(names)}
        }}
    }}

    /// What it types on its own: after a space, or pressed twice.
    pub fn spacing(self) -> char {{
        match self {{
{chr(10).join(spacings)}
        }}
    }}

    /// Each letter it goes on and what it makes of it, by letter.
    pub(crate) fn letters(self) -> &'static [(char, char)] {{
        match self {{
{chr(10).join(arms)}
        }}
    }}
}}

{chr(10).join(tables)}
"""
    with open(os.path.join(HERE, "src", "dead.rs"), "w", encoding="utf-8") as out:
        out.write(text)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    tree = sys.argv[1]
    xkb = Xkb(tree)
    tree_version = version(tree)
    described = []
    for file, layout, variant in LAYOUTS:
        described.append((file, write_keymap(xkb, tree_version, file, layout, variant)))
    write_layouts(described)
    write_dead()
    # As `ob fmt` would leave them.
    root = os.path.normpath(os.path.join(HERE, "..", "..", ".."))
    subprocess.run(["rustfmt", "--edition", "2024", "--config-path",
                    os.path.join(root, "rustfmt.toml"),
                    os.path.join(HERE, "src", "layouts.rs"), os.path.join(HERE, "src", "dead.rs")],
                   cwd=root, check=True)
    for file, description in described:
        print(f"{file:16} {description}")


if __name__ == "__main__":
    main()
