// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::request::{Answer, REQUEST_MAX, Request};
use super::*;

const PASSWD_TEXT: &str = "root:x:0:0:root:/:/system/commands/nukesh
# a note an administrator left
alice:x:1000:1000:Alice Liddell:/users/alice:/system/commands/nukesh
broken line
";

#[test]
fn files_keep_what_they_do_not_read() {
    let mut users = users(PASSWD_TEXT);
    assert_eq!(users.entries().count(), 2);
    let alice = users.entries().find(|u| u.name == "alice").unwrap();
    assert_eq!((alice.uid, alice.gid, alice.full_name.as_str()), (1000, 1000, "Alice Liddell"));
    assert!(alice.is_person() && !users.entries().next().unwrap().is_person());
    users.push(User {
        name: "bob".into(),
        uid: 1001,
        gid: 1001,
        full_name: String::new(),
        home: "/users/bob".into(),
        shell: SHELL.into(),
    });
    assert_eq!(
        users.text(),
        format!("{PASSWD_TEXT}bob:x:1001:1001::/users/bob:/system/commands/nukesh\n")
    );
}

#[test]
fn groups_and_administrators() {
    let mut groups = groups("root:x:0:\nadmins:x:10:alice,carol\nbob:x:1001:\n");
    let group = |name: &str| groups.entries().find(|g| g.name == name).cloned().unwrap();
    assert_eq!(group("admins").members, ["alice", "carol"]);
    assert!(group("bob").members.is_empty());
    let user = |name: &str, uid| User {
        name: name.into(),
        uid,
        gid: uid,
        full_name: String::new(),
        home: String::new(),
        shell: String::new(),
    };
    assert!(is_admin(&user("alice", 1000), &groups));
    assert!(is_admin(&user("root", 0), &groups));
    assert!(!is_admin(&user("bob", 1001), &groups));
    groups.entries_mut().find(|g| g.name == "admins").unwrap().members.push("bob".into());
    assert!(is_admin(&user("bob", 1001), &groups));
    assert!(groups.text().contains("admins:x:10:alice,carol,bob\n"));
}

#[test]
fn new_accounts_take_the_lowest_free_number() {
    let users = users("root:x:0:0::/:/bin/sh\na:x:1000:1000::/users/a:/bin/sh\n");
    let groups = groups("root:x:0:\na:x:1000:\nstaff:x:1001:\n");
    assert_eq!(free_number(&users, &groups), Some(1002));
}

#[test]
fn adding_accounts() {
    let mut users = users("root:x:0:0:root:/:/system/commands/nukesh\n");
    let mut groups = groups("root:x:0:\n");
    let alice = add(&mut users, &mut groups, "alice", "Alice Liddell", true).unwrap();
    assert_eq!((alice.uid, alice.gid, alice.home.as_str()), (1000, 1000, "/users/alice"));
    let bob = add(&mut users, &mut groups, "bob", "", false).unwrap();
    assert_eq!(bob.uid, 1001);
    assert!(is_admin(&alice, &groups) && !is_admin(&bob, &groups));
    assert_eq!(groups.text(), "root:x:0:\nadmins:x:10:alice\nalice:x:1000:\nbob:x:1001:\n");
    assert!(users.text().ends_with(
        "alice:x:1000:1000:Alice Liddell:/users/alice:/system/commands/nukesh\n\
         bob:x:1001:1001::/users/bob:/system/commands/nukesh\n"
    ));
    for (name, full_name) in
        [("alice", ""), ("root", ""), ("admins", ""), ("Eve", ""), ("eve", "a:b")]
    {
        assert!(add(&mut users, &mut groups, name, full_name, false).is_err(), "{name}");
    }
    // The administrators' number taken by another group: no account.
    let mut groups = self::groups("staff:x:10:\n");
    assert!(add(&mut users, &mut groups, "carol", "", true).unwrap_err().contains("another group"));
}

#[test]
fn password_lines() {
    let table = passwords("alice:$argon2id$v=19$x:\nbob::\ncarol:!:19000:0:99999:7:::\n");
    let hashes: Vec<(&str, &str)> =
        table.entries().map(|p| (p.name.as_str(), p.hash.as_str())).collect();
    assert_eq!(hashes, [("alice", "$argon2id$v=19$x"), ("bob", ""), ("carol", "!")]);
    // Fields after the hash, as /etc/shadow's, are not kept: authd
    // writes none.
    assert_eq!(table.text(), "alice:$argon2id$v=19$x:\nbob::\ncarol:!:\n");
}

#[test]
fn names() {
    for good in ["alice", "_build", "x", "mary-jane", "j.r", "user2"] {
        assert_eq!(check_name(good), Ok(()), "{good}");
    }
    for bad in ["", "Alice", "2fast", "-x", "a b", "a:b", "été", "a/b", &"a".repeat(33)] {
        assert!(check_name(bad).is_err(), "{bad}");
    }
    assert!(check_full_name("Alice Liddell, Room 2").is_ok());
    assert!(check_full_name("a:b").is_err() && check_full_name("a\nb").is_err());
}

#[test]
fn passwords_get_warnings_and_no_rules() {
    assert!(password_warning("alice", "").unwrap().contains("anyone at this machine"));
    assert!(password_warning("alice", "abc").unwrap().contains("3 characters"));
    assert_eq!(password_warning("alice", "correct horse"), None);
    // Characters, not bytes.
    assert!(password_warning("alice", "ééééééé").is_some());
    assert_eq!(password_warning("alice", "éééééééé"), None);
}

#[test]
fn requests_go_and_come_back() {
    let requests = [
        Request::Check { name: "alice".into(), password: b"s3cret".to_vec() },
        Request::Passwd { name: "alice".into(), current: Vec::new(), new: b"new".to_vec() },
        Request::Add {
            name: "bob".into(),
            full_name: "Bob Smith".into(),
            admin: true,
            password: b"\xffnot utf-8".to_vec(),
        },
    ];
    for request in requests {
        assert_eq!(Request::decode(&request.encode()), Ok(request));
    }
    assert_eq!(
        Request::Check { name: "a".into(), password: b"b".to_vec() }.encode(),
        b"check\0a\0b\0"
    );
    for bad in
        [&b"check\0a\0"[..], b"check\0a\0b", b"check\0a\0b\0c\0", b"add\0a\0b\0root\0c\0", b""]
    {
        assert!(Request::decode(bad).is_err(), "{bad:?}");
    }
    assert!(Request::decode(&vec![b'a'; REQUEST_MAX + 1]).is_err());
}

#[test]
fn debug_leaves_out_passwords() {
    let request = Request::Check { name: "alice".into(), password: b"hunter2".to_vec() };
    assert!(!format!("{request:?}").contains("hunter2"));
}

#[test]
fn answers() {
    for answer in [
        Answer::Done(String::new()),
        Answer::Done("made bob".into()),
        Answer::Refused("wrong".into()),
    ] {
        assert_eq!(Answer::decode(&answer.encode()), answer);
    }
    assert_eq!(Answer::Refused("a\nb".into()).encode(), "no a b\n");
    assert!(matches!(Answer::decode("what"), Answer::Refused(_)));
}

#[test]
fn wiping_leaves_nothing() {
    let mut secret = b"hunter2".to_vec();
    wipe(&mut secret);
    assert!(secret.is_empty());
}

#[cfg(feature = "hash")]
mod hashes {
    use crate::hash::{Cost, hash, readable, verify};

    const SALT: [u8; 16] = *b"0123456789abcdef";

    #[test]
    fn verifies_what_it_hashed() {
        let phc = hash(b"correct horse", &SALT, Cost::TEST).unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=8192,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$"));
        assert_eq!(verify(b"correct horse", &phc), Ok(true));
        assert_eq!(verify(b"correct horsf", &phc), Ok(false));
        assert_eq!(verify(b"", &phc), Ok(false));
        let empty = hash(b"", &SALT, Cost::TEST).unwrap();
        assert_eq!(verify(b"", &empty), Ok(true));
    }

    /// RFC 9106's test vector needs a secret and associated data, which
    /// PHC strings have no room for; these are the reference
    /// implementation's, libargon2 20190702 as Python's argon2-cffi
    /// bundles it (`hash_secret(b"password", b"somesalt", time_cost=2,
    /// memory_cost=65536, parallelism=4, hash_len=32, type=Type.ID)`).
    #[test]
    fn agrees_with_the_reference_implementation() {
        let phc = "$argon2id$v=19$m=65536,t=2,p=4$c29tZXNhbHQ$\
                   GpZ3sK/oH9p7VIiV56G/64Zo/8GaUw434IimaPqxwCo";
        assert_eq!(verify(b"password", phc), Ok(true));
        assert_eq!(verify(b"Password", phc), Ok(false));
        assert_eq!(
            hash(b"correct horse", &SALT, Cost::TEST).unwrap(),
            "$argon2id$v=19$m=8192,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$\
             64zw4y6hGNSGW1HTKDajc74EIkJpCJ+AUBXDspyrFEU"
        );
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        for bad in [
            "",
            "!",
            "$argon2i$v=19$m=8192,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=16$m=8192,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=19$m=8192,t=1,q=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAA",
            "$argon2id$v=19$m=99999999,t=1,p=1$MDEyMzQ1Njc4OWFiY2RlZg$AAAAAAAAAAAAAAAAAAAAAA",
            "$6$salt$sha512crypt",
        ] {
            assert!(!readable(bad), "{bad}");
            assert!(verify(b"x", bad).is_err(), "{bad}");
        }
    }
}
