// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;

/// What `code` types alone, with Shift, with AltGr, with Shift and AltGr.
fn levels(keymap: &Keymap, code: u16) -> [Sym; 4] {
    [
        keymap.sym(code, false, false, false),
        keymap.sym(code, true, false, false),
        keymap.sym(code, false, true, false),
        keymap.sym(code, true, true, false),
    ]
}

fn ch(c: char) -> Sym {
    Sym::Char(c)
}

#[test]
fn every_layout_parses_with_its_own_name() {
    let mut names = Vec::new();
    for &(layout, text) in LAYOUTS {
        let keymap = Keymap::parse(text).unwrap_or_else(|err| panic!("{layout}: {err}"));
        assert!(text.contains(&format!("# {layout}: {}, made by", keymap.name)), "{layout}");
        // Every layout has a space bar.
        assert_eq!(keymap.sym(KEY_SPACE, false, false, false), ch(' '), "{layout}");
        names.push(keymap.name);
    }
    names.sort();
    names.dedup();
    assert_eq!(names.len(), LAYOUTS.len(), "two layouts have one name");
    assert_eq!(LAYOUTS[0].0, "us");
}

#[test]
fn us_is_linux_us() {
    let us = Keymap::us();
    assert_eq!(us.name, "English (US)");
    assert!(!us.altgr);
    let rows = [
        (KEY_1, '1', '!'),
        (KEY_2, '2', '@'),
        (KEY_6, '6', '^'),
        (KEY_0, '0', ')'),
        (KEY_MINUS, '-', '_'),
        (KEY_EQUAL, '=', '+'),
        (KEY_Q, 'q', 'Q'),
        (KEY_LEFTBRACE, '[', '{'),
        (KEY_RIGHTBRACE, ']', '}'),
        (KEY_SEMICOLON, ';', ':'),
        (KEY_APOSTROPHE, '\'', '"'),
        (KEY_GRAVE, '`', '~'),
        (KEY_BACKSLASH, '\\', '|'),
        (KEY_Z, 'z', 'Z'),
        (KEY_M, 'm', 'M'),
        (KEY_COMMA, ',', '<'),
        (KEY_DOT, '.', '>'),
        (KEY_SLASH, '/', '?'),
        (KEY_SPACE, ' ', ' '),
        (KEY_102ND, '<', '>'),
    ];
    for (code, plain, shifted) in rows {
        assert_eq!(levels(&us, code)[..2], [ch(plain), ch(shifted)], "key {code}");
        // Right Alt is Alt here: it does not change what a key types.
        assert_eq!(us.sym(code, true, true, false), ch(shifted), "key {code}");
    }
    assert_eq!(us.keypad_decimal(), '.');
    // A key the layout does not give types nothing.
    assert_eq!(us.sym(KEY_ENTER, false, false, false), Sym::None);
    assert_eq!(us.sym(KEY_RO, false, false, false), Sym::None);
}

#[test]
fn caps_lock_shifts_letters_only() {
    let us = Keymap::us();
    assert_eq!(us.sym(KEY_A, false, false, true), ch('A'));
    assert_eq!(us.sym(KEY_A, true, false, true), ch('a'));
    assert_eq!(us.sym(KEY_1, false, false, true), ch('1'));
    assert_eq!(us.sym(KEY_SLASH, false, false, true), ch('/'));
    let de = Keymap::builtin("de").unwrap();
    assert_eq!(de.sym(KEY_SEMICOLON, false, false, true), ch('Ö'));
    // ß has no capital of one letter on that key's second level.
    assert_eq!(de.sym(KEY_MINUS, false, false, true), ch('ß'));
    // Caps Lock goes on AltGr's levels when they are a letter pair.
    assert_eq!(de.sym(KEY_O, false, true, true), ch('Ø'));
    assert_eq!(de.sym(KEY_Q, false, true, true), ch('@'));
}

#[test]
fn german_has_altgr_and_dead_keys() {
    let de = Keymap::builtin("de").unwrap();
    assert_eq!(de.name, "German");
    assert!(de.altgr);
    assert_eq!(levels(&de, KEY_Y), [ch('z'), ch('Z'), ch('←'), ch('¥')]);
    assert_eq!(levels(&de, KEY_Q)[2], ch('@'));
    assert_eq!(levels(&de, KEY_102ND)[2], ch('|'));
    assert_eq!(levels(&de, KEY_8)[2], ch('['));
    assert_eq!(
        levels(&de, KEY_EQUAL),
        [
            Sym::Dead(Dead::Acute),
            Sym::Dead(Dead::Grave),
            Sym::Dead(Dead::Cedilla),
            Sym::Dead(Dead::Ogonek)
        ]
    );
    assert_eq!(levels(&de, KEY_GRAVE)[0], Sym::Dead(Dead::Circumflex));
    // The space bar has one level: Shift and AltGr leave it a space.
    assert_eq!(levels(&de, KEY_SPACE), [ch(' '); 4]);
    // Its keypad's decimal key types a comma with Num Lock on.
    assert_eq!(de.keypad_decimal(), ',');
}

#[test]
fn other_layouts_put_keys_where_their_countries_do() {
    let at =
        |layout: &str, code: u16| Keymap::builtin(layout).unwrap().sym(code, false, false, false);
    assert_eq!(at("fr", KEY_Q), ch('a'));
    assert_eq!(at("fr", KEY_1), ch('&'));
    assert_eq!(at("fr", KEY_LEFTBRACE), Sym::Dead(Dead::Circumflex));
    assert_eq!(at("us-dvorak", KEY_E), ch('.'));
    assert_eq!(at("us-colemak", KEY_E), ch('f'));
    assert_eq!(at("es", KEY_SEMICOLON), ch('ñ'));
    assert_eq!(at("pl", KEY_A), ch('a'));
    assert_eq!(Keymap::builtin("pl").unwrap().sym(KEY_A, false, true, false), ch('ą'));
    assert_eq!(at("jp", KEY_YEN), ch('\\'));
    assert_eq!(at("us-intl", KEY_APOSTROPHE), Sym::Dead(Dead::Acute));
    assert_eq!(at("tr", KEY_I), ch('ı'));
    assert_eq!(Keymap::builtin("nowhere"), None);
}

#[test]
fn dead_keys_compose_letters() {
    assert_eq!(compose(Dead::Acute, 'e'), Some('é'));
    assert_eq!(compose(Dead::Circumflex, 'E'), Some('Ê'));
    assert_eq!(compose(Dead::Diaeresis, 'y'), Some('ÿ'));
    assert_eq!(compose(Dead::Caron, 's'), Some('š'));
    assert_eq!(compose(Dead::Ogonek, 'a'), Some('ą'));
    assert_eq!(compose(Dead::Cedilla, 'c'), Some('ç'));
    assert_eq!(compose(Dead::AboveRing, 'A'), Some('Å'));
    assert_eq!(compose(Dead::Tilde, 'n'), Some('ñ'));
    assert_eq!(compose(Dead::DoubleAcute, 'o'), Some('ő'));
    assert_eq!(compose(Dead::BelowComma, 't'), Some('ț'));
    // A space types the accent alone; a letter it does not go on, nothing.
    assert_eq!(compose(Dead::Acute, ' '), Some('´'));
    assert_eq!(compose(Dead::Circumflex, ' '), Some('^'));
    assert_eq!(compose(Dead::Acute, 'q'), None);
    assert_eq!(compose(Dead::Acute, '1'), None);
}

#[test]
fn every_dead_key_has_a_name_and_its_letters_in_order() {
    for dead in Dead::ALL {
        let letters = dead.letters();
        assert!(!letters.is_empty(), "{dead:?}");
        assert!(letters.windows(2).all(|pair| pair[0].0 < pair[1].0), "{dead:?}");
        let file = format!("name = Accents\nq dead:{}\n", dead.name());
        let keymap = Keymap::parse(&file).unwrap();
        assert_eq!(keymap.sym(KEY_Q, false, false, false), Sym::Dead(dead));
        assert!(!dead.spacing().is_control());
    }
    // The layouts use the dead keys there are, and those only.
    for &(layout, text) in LAYOUTS {
        for word in text.split_whitespace().filter_map(|word| word.strip_prefix("dead:")) {
            assert!(Dead::ALL.iter().any(|dead| dead.name() == word), "{layout}: {word}");
        }
    }
}

#[test]
fn a_file_of_ones_own() {
    let text = "# Mine.\n\nname = Mine\nrightalt = altgr\nq a A U+00E6 -\n  w   dead:tilde\n";
    let keymap = Keymap::parse(text).unwrap();
    assert_eq!(keymap.name, "Mine");
    assert!(keymap.altgr);
    assert_eq!(levels(&keymap, KEY_Q), [ch('a'), ch('A'), ch('æ'), Sym::None]);
    assert_eq!(levels(&keymap, KEY_W), [Sym::Dead(Dead::Tilde); 4]);
    assert_eq!(keymap.sym(KEY_E, false, false, false), Sym::None);
    assert_eq!(keymap.keypad_decimal(), '.');
}

#[test]
fn broken_files_are_refused_by_line() {
    let cases = [
        ("name = A\nkey a A\n", 2, "there is no key called key"),
        ("name = A\nq ab\n", 2, "`ab` is not one character"),
        ("name = A\nq U+D800\n", 2, "U+D800 is not a character"),
        ("name = A\nq U+0007\n", 2, "U+0007 is a control character"),
        ("name = A\nq U+12\n", 2, "`U+12` is not one character"),
        ("name = A\nq a\n\nq b\n", 4, "q is given twice"),
        ("name = A\nq a b c d e\n", 2, "a key has at most 4 levels"),
        ("name = A\nq dead:umlaut\n", 2, "there is no dead key called umlaut"),
        ("name = A\nrightalt = left\n", 2, "rightalt is altgr or alt"),
        ("name =\n", 1, "a name is 1 to 64 characters"),
        ("name = A\nq\n", 2, "q has no levels"),
        ("q a A\n", 0, "it has no name line"),
    ];
    for (text, line, what) in cases {
        assert_eq!(Keymap::parse(text), Err(Error { line, what: what.into() }), "{text:?}");
    }
    let big = format!("name = A\n{}", "#".repeat(FILE_MAX));
    assert_eq!(Keymap::parse(&big).unwrap_err().line, 0);
    assert_eq!(
        Error { line: 3, what: "q is given twice".into() }.to_string(),
        "line 3: q is given twice"
    );
}

#[test]
fn any_text_is_safe() {
    // Real layouts cut, spliced and changed at random: each parses or is
    // refused, and what parses answers for every key.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |below: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % below as u64) as usize
    };
    let pieces =
        ["-", "dead:acute", "U+00E9", "U+", "é", "name", "=", "rightalt", "altgr", "\n", " ", "q"];
    for _ in 0..2000 {
        let (_, text) = LAYOUTS[next(LAYOUTS.len())];
        let mut chars: Vec<char> = text.chars().collect();
        for _ in 0..next(8) {
            let at = next(chars.len() + 1);
            match next(3) {
                0 if at < chars.len() => {
                    chars.remove(at);
                }
                1 => chars.splice(at..at, pieces[next(pieces.len())].chars()).for_each(drop),
                _ => chars.truncate(at),
            }
        }
        let text: String = chars.into_iter().collect();
        if let Ok(keymap) = Keymap::parse(&text) {
            for code in 0..=KEY_YEN {
                for i in 0..16 {
                    keymap.sym(code, i & 1 != 0, i & 2 != 0, i & 4 != 0);
                }
            }
            keymap.keypad_decimal();
        }
    }
}

#[test]
fn the_setting_names_a_layout() {
    assert_eq!(layout_setting("").unwrap(), "us");
    assert_eq!(layout_setting(&setting_text("de")).unwrap(), "de");
    assert_eq!(layout_setting("# Mine.\n  layout =  us-intl \n").unwrap(), "us-intl");
    let refused = |text: &str| layout_setting(text).unwrap_err();
    assert_eq!(refused("layout = ../etc/x\n").line, 1);
    assert_eq!(refused("\nlayout = De\n").line, 2);
    assert_eq!(refused("speed = 3\n").what, "the only setting is layout");
    assert_eq!(refused("layout\n").what, "a setting is `layout = NAME`");
    assert!(valid_name("ch-fr") && valid_name("us"));
    assert!(
        !valid_name("") && !valid_name("a/b") && !valid_name(".x") && !valid_name(&"a".repeat(33))
    );
    assert!(LAYOUTS.iter().all(|&(name, _)| valid_name(name)));
    assert_eq!(load("../x"), Err("`../x` cannot name a layout".into()));
}
