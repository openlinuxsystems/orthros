// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};

use super::locate::{Step, locate};
use super::*;

/// The security design's worked examples of manifest version 1.
const EXAMPLES: &[(&str, &str)] = &[
    ("nginx", include_str!("examples/nginx.json")),
    ("postgresql", include_str!("examples/postgresql.json")),
    ("git", include_str!("examples/git.json")),
    ("irssi", include_str!("examples/irssi.json")),
];

/// Its examples of what later versions bring.
const LATER: &[(&str, &str)] = &[
    ("node", include_str!("examples/node-version-2.json")),
    ("photos", include_str!("examples/photos-version-2.json")),
];

/// A manifest with the fields it must have, then `more`, which starts
/// with a comma.
fn with(more: &str) -> String {
    format!(
        r#"{{
  "manifest_version": 1,
  "id": "org.example.app",
  "name": "App",
  "summary": "Does things",
  "version": "1.0",
  "publisher": {{ "name": "Example" }},
  "license": "MIT",
  "requires_os": "1.0",
  "architectures": ["x86_64"],
  "programs": {{ "app": {{ "path": {{ "x86_64": "bin/app" }} }} }}{more}
}}
"#
    )
}

/// The errors `text` gets, each as `bundle check` says it.
fn errors(text: &str) -> Vec<String> {
    match from_json(text) {
        Ok(_) => vec![],
        Err(errors) => errors.iter().map(ToString::to_string).collect(),
    }
}

/// The one error `text` gets.
fn error(text: &str) -> String {
    let errors = errors(text);
    assert_eq!(errors.len(), 1, "{text}: {errors:?}");
    errors.into_iter().next().unwrap_or_default()
}

#[test]
fn the_worked_examples_read() {
    for (name, text) in EXAMPLES {
        let manifest = from_json(text).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert!(manifest.id.0.ends_with(&format!(".{name}")), "{name}");
        assert_eq!(manifest.architectures, [Arch::X86_64]);
        assert!(manifest.notes().is_empty(), "{name}: {:?}", manifest.notes());
    }
    let nginx = from_json(EXAMPLES[0].1).unwrap();
    assert_eq!(nginx.version.0, "1.27.3");
    assert_eq!(nginx.publisher.url.as_ref().map(|u| u.0.as_str()), Some("https://dukovo.org"));
    let service = &nginx.services[0];
    assert_eq!(service.arguments, ["-g", "daemon off;"]);
    assert_eq!(service.account.as_ref().map(|a| a.0.as_str()), Some("nginx"));
    assert_eq!(service.restart, Restart::OnFailure);
    assert_eq!(service.limits.memory, Some(512 << 20));
    assert_eq!(service.limits.open_files, Some(65536));
    assert_eq!(service.limits.cpu_weight, 100);
    assert_eq!(service.start, Start::WhenEnabled);
    let listen = nginx.permissions.get("network.listen").unwrap();
    assert_eq!(listen.need, Need::Required);
    assert_eq!(listen.ports.iter().map(ToString::to_string).collect::<Vec<_>>(), ["80", "443"]);
    let shared = nginx.permissions.get("files.shared").unwrap();
    assert_eq!(shared.folders, [("www".to_string(), Access::Read)]);
    assert_eq!(nginx.uses[0].interface.0, "dukovo.certificates/1");
    assert_eq!(nginx.data, Data { settings: true, data: false, cache: true });

    let postgres = from_json(EXAMPLES[1].1).unwrap();
    assert_eq!(postgres.programs.len(), 3);
    assert_eq!(postgres.provides[0].socket.as_ref().map(|s| s.0.as_str()), Some("postgresql"));
    assert_eq!(postgres.provides[0].to, To::Approved);

    let irssi = from_json(EXAMPLES[3].1).unwrap();
    let folder = irssi.permissions.get("files.folder").unwrap();
    assert_eq!(folder.folders, [("downloads".to_string(), Access::ReadWrite)]);
    assert_eq!(irssi.commands[0].runs_with, RunsWith::Sandbox);
}

#[test]
fn a_later_version_is_refused_for_what_it_is() {
    for (name, text) in LATER {
        assert_eq!(
            error(text),
            "line 2: this bundle is for manifest version 2, and this Dukovo reads version 1: \
             update Dukovo to install it",
            "{name}"
        );
    }
    assert_eq!(
        error(&with("").replace("\"manifest_version\": 1", "\"manifest_version\": 0")),
        "line 2: manifest_version is 1, not 0"
    );
    assert_eq!(
        error(&with("").replace("\"manifest_version\": 1", "\"manifest_version\": \"1\"")),
        "line 2: manifest_version is 1, not \"1\""
    );
    assert_eq!(
        error(&with("").replace("\"manifest_version\": 1,", "")),
        "it has no manifest_version: add \"manifest_version\": 1"
    );
}

#[test]
fn what_a_manifest_leaves_out() {
    let manifest = from_json(&with("")).unwrap();
    assert!(manifest.description.is_none() && manifest.source.is_none());
    assert!(manifest.commands.is_empty() && manifest.services.is_empty());
    assert!(manifest.permissions.0.is_empty() && manifest.provides.is_empty());
    assert_eq!(manifest.data, Data::default());
    assert_eq!(manifest.requires_os, OsVersion(1, 0, 0));
    for key in REQUIRED.iter().filter(|k| **k != "manifest_version") {
        // The field renamed to a vendor's own, which is read past.
        let text = with("").replace(&format!("\n  \"{key}\":"), &format!("\n  \"x-{key}\":"));
        assert_eq!(error(&text), format!("it has no {key}; manifest.json(5) says what it holds"));
    }
}

#[test]
fn unknown_reserved_and_vendor_fields() {
    assert_eq!(
        error(&with(",\n  \"lines\": 3")),
        "line 12: lines is not a field of a manifest; manifest.json(5) lists them, and a \
         vendor's own fields start with x-"
    );
    assert_eq!(
        error(&with(",\n  \"entries\": {}")),
        "line 12: entries comes in a later manifest version: version 1 reserves its name"
    );
    for reserved in RESERVED {
        assert!(error(&with(&format!(",\n  \"{reserved}\": 1"))).contains("reserves its name"));
    }
    // A vendor's own fields and $schema are read past, whatever they hold.
    let text = with(",\n  \"x-acme\": { \"anything\": [1, null] },\n  \"$schema\": \"https://x\"");
    assert!(from_json(&text).is_ok());
    assert!(error(&with(",\n  \"$schema\": 1")).starts_with("line 12: "));
    assert_eq!(error(&with(",\n  \"name\": \"Again\"")), "line 12: name is written twice");
    assert_eq!(
        error(
            &with("")
                .replace("{ \"name\": \"Example\" }", "{ \"name\": \"Example\", \"mail\": \"a\" }")
        ),
        "line 7: `mail` is not a field here; manifest.json(5) lists them"
    );
    assert_eq!(error("[2]"), "line 1: a manifest is an object, between { and }");
    assert_eq!(
        error(&with(",\n  \"update\": 1")),
        "line 12: integer `1`, expected an object for Update"
    );
    // What a manifest says is never written to the terminal as it is.
    let message = error(&with(",\n  \"\\u001b[2Jx\": 1"));
    assert!(
        message.contains("\\u{1b}[2Jx is not a field") && !message.contains('\u{1b}'),
        "{message}"
    );
    let message = error(&with(",\n  \"icon\": \"a\\u0007.svg\""));
    assert!(
        message.contains("is not a path in the bundle") && !message.contains('\u{7}'),
        "{message}"
    );
    assert!(error("{").starts_with("line 1: "));
    assert!(!error("").is_empty());
}

#[test]
fn the_fields_check_what_they_hold() {
    let bad = |field: &str, old: &str, new: &str| {
        let text = with("").replace(&format!("\"{field}\": {old}"), &format!("\"{field}\": {new}"));
        assert_ne!(text, with(""), "{field}");
        error(&text)
    };
    for id in ["\"App\"", "\"app\"", "\"org..app\"", "\"org.-x\"", "\"org.ex_ample\""] {
        assert!(bad("id", "\"org.example.app\"", id).contains("reverse DNS"), "{id}");
    }
    assert!(bad("name", "\"App\"", "\"\"").contains("this text is empty"));
    assert!(bad("name", "\"App\"", "\"A\\nB\"").contains("is one line"));
    let long = format!("\"{}\"", "a".repeat(201));
    assert!(bad("summary", "\"Does things\"", &long).contains("longer than 200"));
    for version in
        ["\"1\"", "\"17.2\"", "\"1.27.3\"", "\"1.2.3.4\"", "\"1.0.0-beta.1\"", "\"1.0+git.5\""]
    {
        let text = with("").replace("\"version\": \"1.0\"", &format!("\"version\": {version}"));
        assert!(from_json(&text).is_ok(), "{version}");
    }
    for version in
        ["\"\"", "\"v1\"", "\"01.2\"", "\"1..2\"", "\"1.2.3.4.5\"", "\"1.0-\"", "\"1.0+a+b\""]
    {
        assert!(bad("version", "\"1.0\"", version).contains("numbers between dots"), "{version}");
    }
    assert!(bad("version", "\"1.0\"", "1").starts_with("line 6: "));
    for os in ["\"1\"", "\"1.0.0.0\"", "\"1.x\"", "\"1..0\""] {
        assert!(bad("requires_os", "\"1.0\"", os).contains("Dukovo release"), "{os}");
    }
    let text = with("").replace("\"requires_os\": \"1.0\"", "\"requires_os\": \"1.4.1\"");
    assert_eq!(from_json(&text).unwrap().requires_os.to_string(), "1.4.1");
    assert!(bad("license", "\"MIT\"", "\"MIT AND\"").contains("ends where a license should be"));
    assert!(bad("license", "\"MIT\"", "\"MIT Apache-2.0\"").contains("need `AND` or `OR`"));
    assert!(
        bad("architectures", "[\"x86_64\"]", "[\"arm\"]")
            .contains("architectures must be x86_64, aarch64, riscv64 or wasm, not `arm`")
    );
    let text = with(",\n  \"homepage\": \"http://example.org\"");
    assert!(error(&text).contains("not a web address starting https://"));
    let text = with(",\n  \"icon\": \"/icon.svg\"");
    assert!(error(&text).contains("is not a path in the bundle"));
    for path in ["\"../app\"", "\"bin//app\"", "\"bin/./app\"", "\"\"", "\"bin\\\\app\""] {
        let text = with("").replace("\"bin/app\"", path);
        assert!(error(&text).contains("is not a path in the bundle"), "{path}");
    }
}

#[test]
fn programs_and_architectures_agree() {
    let two = with("").replace("[\"x86_64\"]", "[\"x86_64\", \"aarch64\"]");
    assert_eq!(error(&two), "line 11: program app has no path for aarch64");
    let text = with("").replace("[\"x86_64\"]", "[\"x86_64\", \"x86_64\"]");
    assert_eq!(error(&text), "line 10: architectures names x86_64 twice");
    let text = with("")
        .replace("\"x86_64\": \"bin/app\"", "\"x86_64\": \"bin/app\", \"wasm\": \"app.wasm\"");
    assert_eq!(
        error(&text),
        "line 11: program app has a path for wasm, which architectures does not name"
    );
    let text = with("").replace("[\"x86_64\"]", "[]");
    assert_eq!(errors(&text)[0], "line 10: architectures names at least one, such as x86_64");
    let text = with("").replace("{ \"app\": { \"path\": { \"x86_64\": \"bin/app\" } } }", "{}");
    assert_eq!(error(&text), "line 11: programs names at least one program");
    let text = with("").replace("\"bin/app\" }", "\"bin/app\", \"x86_64\": \"bin/b\" }");
    assert!(error(&text).contains("x86_64 is written twice"));
}

#[test]
fn commands_and_services_run_its_programs() {
    let command = |name: &str, program: &str| {
        format!(
            "{{ \"name\": \"{name}\", \"program\": \"{program}\", \"runs_with\": \"sandbox\" }}"
        )
    };
    let text = with(&format!(
        ",\n  \"commands\": [\n    {},\n    {}\n  ]",
        command("app", "app"),
        command("app", "tool")
    ));
    assert_eq!(
        errors(&text),
        [
            "line 14: command app is named twice",
            "line 14: command app runs tool, which programs does not name",
        ]
    );
    let text = with(",\n  \"commands\": [ { \"name\": \"app\", \"program\": \"app\" } ]");
    assert_eq!(error(&text), "line 12: missing field `runs_with`");
    let text = with(&format!(",\n  \"commands\": [ {} ]", command("-app", "app")));
    assert!(error(&text).contains("is not a name"));

    let service = with(
        ",\n  \"services\": [ {\n    \"name\": \"app\",\n    \"program\": \"app\",\n    \
         \"health\": { \"socket\": \"app\" }\n  } ]",
    );
    assert_eq!(
        error(&service),
        "line 15: service app is checked through the socket app, which provides does not name"
    );
    let provided = service.replace(
        "\n}\n",
        ",\n  \"provides\": [ { \"socket\": \"app\", \"to\": \"anyone\" } ]\n}\n",
    );
    let manifest = from_json(&provided).unwrap();
    let svc = &manifest.services[0];
    assert_eq!(
        (svc.restart, svc.start, svc.account.clone()),
        (Restart::Always, Start::WhenEnabled, None)
    );
    assert_eq!(svc.limits, Limits::default());

    let in_service = |more: &str| {
        error(&with(&format!(
            ",\n  \"services\": [ {{ \"name\": \"app\", \"program\": \"app\", {more} }} ]"
        )))
    };
    assert!(
        in_service("\"limits\": { \"open_files\": 0 }")
            .contains("open_files is from 1 to 1048576, not 0")
    );
    assert!(
        in_service("\"limits\": { \"cpu_weight\": 0 }")
            .contains("cpu_weight is from 1 to 10000, not 0")
    );
    assert!(in_service("\"limits\": { \"swap\": 0 }").contains("`swap` is not a field here"));
    assert!(in_service("\"secrets\": [\"key\", \"key\"]").contains("secrets names key twice"));
    assert!(
        in_service("\"start\": \"later\"")
            .contains("start must be when-enabled, at-boot or on-connection, not `later`")
    );
    assert!(in_service("\"account\": \"Root\"").contains("is not an account's name"));
    assert!(
        in_service("\"environment\": { \"A=B\": \"c\" }").contains("cannot be a variable's name")
    );
    assert!(in_service("\"needs\": [\"dukovo.network\"]").contains("is not an interface"));
    assert_eq!(
        in_service("\"start\": \"on-connection\""),
        "line 12: service app starts on a connection, but provides names no socket to connect to"
    );
    assert!(
        in_service("\"needs\": [\"a.b/1\", \"a.b/1\"]").contains("service app needs a.b/1 twice")
    );
    let text = with(",\n  \"services\": [ { \"name\": \"Web.Server\", \"program\": \"app\" } ]");
    assert_eq!(
        error(&text),
        "line 12: service Web.Server runs as an account of its name unless it names one, and \
         Web.Server cannot be an account's: add \"account\", such as \"web.server\""
    );
    let text = text.replace("\"program\": \"app\"", "\"program\": \"app\", \"account\": \"web\"");
    assert!(from_json(&text).is_ok());
    let text = with(
        ",\n  \"services\": [\n    { \"name\": \"app\", \"program\": \"app\" },\n    \
         { \"name\": \"app\", \"program\": \"nothing\" }\n  ]",
    );
    assert_eq!(
        errors(&text),
        [
            "line 14: service app is named twice",
            "line 14: service app runs nothing, which programs does not name",
        ]
    );
}

#[test]
fn provides_names_one_thing_each() {
    let provides = |items: &str| with(&format!(",\n  \"provides\": [\n    {items}\n  ]"));
    let text =
        provides("{ \"socket\": \"a\", \"interface\": \"org.example.a/1\", \"to\": \"anyone\" }");
    assert!(error(&text).contains("names a socket or an interface, one of the two"));
    let text = provides("{ \"to\": \"anyone\" }");
    assert!(error(&text).contains("names a socket or an interface, one of the two"));
    let text = provides(
        "{ \"interface\": \"org.example.a/1\", \"to\": \"anyone\" },\n    \
         { \"interface\": \"org.example.a/1\", \"to\": \"system\" }",
    );
    assert_eq!(error(&text), "line 14: provides names it twice");
    let text = provides("{ \"interface\": \"org.example.a/1\", \"to\": \"everyone\" }");
    assert!(error(&text).contains("to must be approved, anyone, publisher or system"));
}

#[test]
fn permissions_take_what_the_list_says() {
    let asking =
        |permissions: &str| with(&format!(",\n  \"permissions\": {{\n    {permissions}\n  }}"));
    let manifest = from_json(&asking(
        "\"network.listen\": { \"ports\": [80, \"8000-8009\"], \"need\": \"required\" },\n    \
         \"files.app-data\": { \"apps\": [\"org.example.other\"], \"need\": \"optional\", \
         \"why\": \"Imports its notes\" },\n    \
         \"admin.logs\": { \"need\": \"optional\", \"why\": \"Shows the service's log\" }",
    ))
    .unwrap();
    let listen = manifest.permissions.get("network.listen").unwrap();
    assert_eq!(
        listen.ports.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["80", "8000-8009"]
    );
    assert_eq!(manifest.permissions.get("files.app-data").unwrap().apps[0].0, "org.example.other");
    assert_eq!(
        manifest.notes(),
        [
            "network.listen has no `why`: people see only the system's words for it, not why App asks"
        ]
    );

    assert_eq!(
        error(&asking("\"network.everything\": { \"need\": \"required\" }")),
        "line 13: network.everything is not a permission; manifest.json(5) lists them"
    );
    assert_eq!(
        error(&asking("\"devices.camera\": { \"need\": \"optional\" }")),
        "line 13: devices.camera comes with Dukovo 1.6.6, in a later manifest version: version 1 \
         reserves its name"
    );
    assert_eq!(error(&asking("\"notifications\": { }")), "line 13: missing field `need`");
    assert_eq!(
        error(&asking("\"network.outbound\": { \"need\": \"required\", \"ports\": [80] }")),
        "line 13: network.outbound takes no ports"
    );
    let cases = [
        ("\"network.listen\": { \"need\": \"required\" }", "names its ports"),
        (
            "\"network.listen\": { \"need\": \"required\", \"ports\": [80, \"79-81\"] }",
            "79-81 overlaps 80",
        ),
        (
            "\"files.folder\": { \"need\": \"required\", \"folders\": { \"secrets\": \"read\" } }",
            "secrets is not one of your folders",
        ),
        (
            "\"files.shared\": { \"need\": \"required\", \"folders\": { \"../x\": \"read\" } }",
            "is not a folder of /shared",
        ),
        (
            "\"files.folder\": { \"need\": \"required\", \"folders\": { \"music\": \"write\" } }",
            "folders must be read or read-write, not `write`",
        ),
        ("\"files.folder\": { \"need\": \"required\", \"folders\": {} }", "names its folders"),
        ("\"files.app-data\": { \"need\": \"required\", \"apps\": [] }", "names the apps"),
        (
            "\"files.app-data\": { \"need\": \"required\", \"apps\": [\"org.a.b\", \"org.a.b\"] }",
            "files.app-data names org.a.b twice",
        ),
        ("\"notifications\": { \"need\": \"sometimes\" }", "need must be required or optional"),
        ("\"notifications\": { \"need\": \"optional\", \"apps\": [\"org.a.b\"] }", "takes no apps"),
        (
            "\"notifications\": { \"need\": \"optional\", \"when\": 1 }",
            "`when` is not a field here",
        ),
        (
            "\"notifications\": { \"need\": \"optional\" },\n    \"notifications\": { \"need\": \"optional\" }",
            "notifications is written twice",
        ),
    ];
    for (permissions, why) in cases {
        let message = error(&asking(permissions));
        assert!(message.contains(why), "{permissions}: {message}");
    }

    // A program narrows itself only to what the bundle asks for.
    let text =
        asking("\"notifications\": { \"need\": \"optional\", \"why\": \"Says when it is done\" }")
            .replace(
                "\"path\": { \"x86_64\": \"bin/app\" }",
                "\"path\": { \"x86_64\": \"bin/app\" }, \"permissions\": [\"network.outbound\"]",
            );
    assert_eq!(
        error(&text),
        "line 11: program app narrows itself to network.outbound, which the bundle does not ask \
         for in permissions"
    );

    // Translations explain only what it asks for.
    let text = with(
        ",\n  \"translations\": {\n    \"de\": { \"name\": \"Anwendung\", \"why\": { \
         \"notifications\": \"Meldet sich\" } }\n  }",
    );
    assert_eq!(
        error(&text),
        "line 13: the de translation explains notifications, which the bundle does not ask for"
    );
    let text = with(
        ",\n  \"uses\": [\n    { \"interface\": \"a.b/1\", \"need\": \"required\" },\n    \
         { \"interface\": \"a.b/1\", \"need\": \"optional\" }\n  ]",
    );
    assert_eq!(error(&text), "line 14: uses names a.b/1 twice");
    let text = with(",\n  \"translations\": { \"German\": {} }");
    assert!(error(&text).contains("is not a language tag"));
}

#[test]
fn every_permission_of_the_list() {
    for permission in permissions::LIST {
        let takes = match permission.takes {
            Takes::Nothing => "",
            Takes::PersonalFolders => ", \"folders\": { \"documents\": \"read\" }",
            Takes::SharedFolders => ", \"folders\": { \"media\": \"read-write\" }",
            Takes::Ports => ", \"ports\": [8080]",
            Takes::Apps => ", \"apps\": [\"org.example.other\"]",
        };
        let text = with(&format!(
            ",\n  \"permissions\": {{ \"{}\": {{ \"need\": \"optional\", \"why\": \"Because\"{takes} }} }}",
            permission.name
        ));
        match permission.reserved_until {
            None => assert!(from_json(&text).is_ok(), "{}: {:?}", permission.name, errors(&text)),
            Some(release) => {
                assert!(error(&text).contains(&format!("comes with Dukovo {release}")));
            }
        }
    }
    // Each name once, and each one's words short enough for a prompt.
    for (i, permission) in permissions::LIST.iter().enumerate() {
        assert!(
            permissions::LIST[..i].iter().all(|p| p.name != permission.name),
            "{}",
            permission.name
        );
        assert!(
            permission.lets.len() <= 70 && !permission.lets.ends_with('.'),
            "{}",
            permission.name
        );
        assert_eq!(permissions::find(permission.name).map(|p| p.name), Some(permission.name));
    }
}

#[test]
fn a_gnu_license_links_to_its_source() {
    let text = with("").replace("\"MIT\"", "\"GPL-3.0-or-later\"");
    assert_eq!(
        error(&text),
        "line 8: a bundle under GPL-3.0-or-later links to its exact source: add \"source\", such \
         as \"https://example.org/source/app-1.0.tar.xz\""
    );
    let text = with("").replace("\"MIT\"", "\"MIT OR LGPL-2.1-only\"");
    assert!(error(&text).contains("LGPL-2.1-only"));
    let text = with(",\n  \"source\": \"https://example.org/app.tar.xz\"")
        .replace("\"MIT\"", "\"AGPL-3.0-only\"");
    assert!(from_json(&text).is_ok());
    let text = with(",\n  \"icon\": \"resources/app.png\"");
    assert_eq!(error(&text), "line 12: icon is an SVG file in the bundle, not resources/app.png");
}

#[test]
fn an_update_feed_and_its_key() {
    // minisign's own example key.
    let key = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    let feed = |key: &str| {
        with(&format!(
            ",\n  \"update\": {{ \"feed\": \"https://example.org/feed\", \"key\": \"{key}\" }}"
        ))
    };
    assert_eq!(from_json(&feed(key)).unwrap().update.unwrap().key.0, key);
    assert!(error(&feed("RWQf6LRCGA9i53mlYecO4IzT")).contains("is not a minisign public key"));
    assert!(error(&feed(&key.replace("RW", "Rw"))).contains("is not a minisign public key"));
    let text = with(",\n  \"update\": { \"feed\": \"https://example.org/feed\" }");
    assert_eq!(error(&text), "line 12: missing field `key`");
}

#[test]
fn spdx_expressions() {
    for (text, ids) in [
        ("MIT", &["MIT"][..]),
        ("MIT OR Apache-2.0", &["MIT", "Apache-2.0"]),
        ("(MIT OR Apache-2.0) AND BSD-3-Clause", &["MIT", "Apache-2.0", "BSD-3-Clause"]),
        ("GPL-2.0-or-later WITH Classpath-exception-2.0", &["GPL-2.0-or-later"]),
        ("GPL-2.0+", &["GPL-2.0+"]),
        ("LicenseRef-Proprietary", &["LicenseRef-Proprietary"]),
        ("DocumentRef-spdx:LicenseRef-Mine", &["DocumentRef-spdx:LicenseRef-Mine"]),
        ("((MIT))", &["MIT"]),
        ("mit or apache-2.0", &["mit", "apache-2.0"]),
    ] {
        assert_eq!(spdx::check(text).as_deref(), Ok(ids), "{text}");
    }
    for (text, why) in [
        ("", "license is empty"),
        ("MIT AND", "it ends where a license should be"),
        ("AND MIT", "an operator stands"),
        ("(MIT", "a `(` is not closed"),
        ("MIT)", "a `)` closes nothing"),
        ("()", "a `)` closes nothing"),
        ("MIT Apache-2.0", "need `AND` or `OR`"),
        ("MIT WITH", "`WITH` takes an exception"),
        ("MIT/Apache-2.0", "is not a license identifier"),
        ("DocumentRef-:LicenseRef-x", "is not a license identifier"),
    ] {
        let err = spdx::check(text).unwrap_err();
        assert!(err.contains(why), "{text}: {err}");
    }
    assert!(spdx::needs_source("GPL-2.0-only") && spdx::needs_source("LGPL-3.0-or-later"));
    assert!(!spdx::needs_source("MIT") && !spdx::needs_source("MPL-2.0"));
}

#[test]
fn locate_finds_keys_and_elements() {
    let text = "{\n \"a\": [1, {\"b\": \"x\\\"]\"}, [2]],\n \"c\": {\"d\": {}, \"e\": 5}\n}";
    let at = |path: &[Step<'_>]| locate(text, path).map(|at| &text[at..at + 3]);
    assert_eq!(locate(text, &[Step::Key("a")]).map(|at| crate::line_of(text, at)), Some(2));
    assert_eq!(
        locate(text, &[Step::Key("c"), Step::Key("e")]).map(|at| crate::line_of(text, at)),
        Some(3)
    );
    assert_eq!(at(&[Step::Key("c"), Step::Key("e")]), Some("\"e\""));
    assert_eq!(at(&[Step::Key("a"), Step::Index(2)]), Some("[2]"));
    assert_eq!(at(&[Step::Key("a"), Step::Index(1), Step::Key("b")]), Some("\"b\""));
    assert_eq!(locate(text, &[Step::Key("a"), Step::Index(3)]), None);
    assert_eq!(locate(text, &[Step::Key("z")]), None);
    assert_eq!(locate(text, &[Step::Index(0)]), None);
    assert_eq!(locate("[]", &[Step::Index(0)]), None);
    assert_eq!(locate("", &[Step::Key("a")]), None);
}

#[test]
fn the_template_reads() {
    let text =
        template("org.example.notes", "Notes \"2\"", "Example Ltd", "LicenseRef-Proprietary");
    let manifest = from_json(&text).unwrap_or_else(|e| panic!("{text}\n{e:?}"));
    assert_eq!(manifest.name.0, "Notes \"2\"");
    assert_eq!(manifest.commands[0].name.0, "notes");
    let program = manifest.programs.get("notes").unwrap();
    assert_eq!(program.path.get(&Arch::X86_64).unwrap().0, "bin/notes");
    assert_eq!(manifest.version.0, "0.1.0");
    assert!(manifest.notes().is_empty());
}

#[cfg(feature = "schema")]
#[test]
fn the_schema_has_every_field_and_permission() {
    let schema: serde_json::Value = serde_json::from_str(&schema()).unwrap();
    assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert_eq!(schema["$id"], "https://dukovo.org/schemas/manifest-1.json");
    let properties = schema["properties"].as_object().unwrap();
    let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
    let mut want: Vec<&str> = KEYS.iter().copied().chain(["$schema"]).collect();
    keys.sort_unstable();
    want.sort_unstable();
    assert_eq!(keys, want);
    let required = schema["required"].as_array().unwrap();
    let mut required: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    let mut want = REQUIRED.to_vec();
    required.sort_unstable();
    want.sort_unstable();
    assert_eq!(required, want);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["patternProperties"]["^x-"], true);

    // The permissions version 1 defines, and no other.
    let defs = schema["$defs"].as_object().unwrap();
    let permissions = defs["Permissions"]["properties"].as_object().unwrap();
    let defined: Vec<&str> =
        permissions::LIST.iter().filter(|p| p.reserved_until.is_none()).map(|p| p.name).collect();
    assert_eq!(permissions.len(), defined.len());
    for name in defined {
        assert!(permissions.contains_key(name), "{name}");
    }
    // Every type the schema refers to, it defines.
    let text = schema.to_string();
    for (at, _) in text.match_indices("\"$ref\":\"#/$defs/") {
        let name = &text[at + 16..];
        let name = &name[..name.find('"').unwrap()];
        assert!(defs.contains_key(name), "{name}");
    }
}

/// A small, fast random number generator (xorshift64*), as nukesh's
/// fuzzing has.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn damaged_manifests_get_errors_not_panics() {
    // The examples with bytes taken out, pieces put in and lines doubled:
    // each reads, or fails with errors on lines the text has. The seed is
    // fixed, so a failure repeats; MANIFEST_FUZZ_SEED changes it.
    let seed = std::env::var("MANIFEST_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let mut rng = Rng(seed.unwrap_or(0x9e37_79b9_7f4a_7c15) | 1);
    let pieces =
        ["{", "}", "[", "]", ",", ":", "\"", "\\", "1", "-", " ", "x-", "null", "\"a\"", "\u{e9}"];
    for round in 0..4000 {
        let (_, base) = EXAMPLES[round % EXAMPLES.len()];
        let mut text = String::from(base);
        for _ in 0..=rng.below(4) {
            let mut at = rng.below(text.len());
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            match rng.below(3) {
                0 => {
                    let mut end = (at + 1 + rng.below(8)).min(text.len());
                    while !text.is_char_boundary(end) {
                        end += 1;
                    }
                    text.replace_range(at..end, "");
                }
                1 => text.insert_str(at, pieces[rng.below(pieces.len())]),
                _ => {
                    let line = text[at..].lines().next().unwrap_or("").to_string();
                    text.insert_str(at, &line);
                }
            }
        }
        if let Err(errors) = from_json(&text) {
            assert!(!errors.is_empty());
            let lines = text.lines().count() + 1;
            for error in &errors {
                assert!(
                    error.line.is_none_or(|line| (1..=lines).contains(&line)),
                    "{text}\n{error}"
                );
                assert!(!error.message.is_empty());
            }
        }
    }
}
