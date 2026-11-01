// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! bundle(1): making, checking, signing and packing bundles
//! (manifest.json(5), ADRs 0221 and 0222), with the reader the system
//! installs bundles with.
//!
//! - `bundle new ID [FOLDER]` writes the manifest of a new bundle, and
//!   makes the developer key that signs your own builds.
//! - `bundle check [FOLDER | FILE]` says what is wrong with one, by line,
//!   and that the programs it names are in the folder.
//! - `bundle schema` prints the JSON Schema of the manifest, for editors.
//! - `bundle upgrade-manifest [FOLDER | FILE]` brings a manifest up to the
//!   newest version this Dukovo reads.
//! - `bundle key` shows the developer key.
//! - `bundle sign [FOLDER]` lists and signs a bundle's files.
//! - `bundle pack [FOLDER]` makes the `.dkv` file of a signed bundle.
//! - `bundle verify [FOLDER | FILE.dkv]` checks one's signature and files.
//! - `bundle unpack FILE.dkv FOLDER` unpacks one, in a process that holds
//!   nothing but the archive and the new folder.
//! - `bundle install FILE.dkv`, `bundle list`, `bundle info ID` and
//!   `bundle remove ID` ask bundled(8), which installs, seals and removes
//!   bundles (ADR 0223).
//! - `bundle grant ID PERMISSION [WORD...]` and `bundle revoke ID
//!   PERMISSION` change what an app may do (ADR 0224): bundled gives a
//!   permission only after the trusted prompt's yes.

mod base64;
pub(crate) mod confine;
pub(crate) mod folder;
pub(crate) mod grants;
pub(crate) mod minisign;
pub(crate) mod protocol;
pub(crate) mod registry;
pub(crate) mod run;
mod sbom;
pub(crate) mod sheet;
pub(crate) mod signed;
pub(crate) mod sums;
mod zip;

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use manifest::bundle::{self, Manifest};

use self::minisign::{PublicKey, SecretKey};
use self::protocol::{Answer, Request};
use self::registry::Place;
use crate::args::{Arg, Args};
use crate::signin;
use crate::util::{self, Fail, fail, usage};

const USAGE: &str =
    "Usage: bundle new ID [FOLDER] [--name NAME] [--publisher NAME] [--license SPDX]
       bundle check [FOLDER | FILE]
       bundle schema
       bundle upgrade-manifest [FOLDER | FILE]
       bundle key
       bundle sign [FOLDER] [--key SECRET-KEY-FILE]
       bundle pack [FOLDER] [--key SECRET-KEY-FILE] [--output FILE]
       bundle verify [FOLDER | FILE.dkv] [--key PUBLIC-KEY]
       bundle unpack FILE.dkv FOLDER
       bundle install FILE.dkv [--for-everyone] [--yes]
       bundle list
       bundle info ID
       bundle remove ID [--for-everyone] [--keep-data | --purge]
       bundle run [--command NAME] ID [ARGUMENT...]
       bundle grant ID PERMISSION [FOLDER=read|read-write | APP-ID]...
       bundle revoke ID PERMISSION";

/// The manifest's name in a bundle's folder.
pub(crate) const MANIFEST: &str = "manifest.json";

/// A packed bundle's extension.
const EXTENSION: &str = "dkv";

/// The developer key's files, in `$XDG_DATA_HOME/bundle`.
const DEVELOPER_KEY: &str = "developer.key";
const DEVELOPER_PUBLIC: &str = "developer.pub";

/// Where random bytes come from.
#[cfg(target_os = "radix")]
pub(crate) const RANDOM: &str = "/devices/urandom";
#[cfg(not(target_os = "radix"))]
pub(crate) const RANDOM: &str = "/dev/urandom";

/// The license `bundle new` writes when none is given: the publisher's
/// own terms, until they choose a license.
const LICENSE: &str = "LicenseRef-Proprietary";

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let subcommand = args.subcommand().map(|s| s.to_string_lossy().into_owned());
    if subcommand.as_deref() == Some("run") {
        return run_main(args);
    }
    let mut operands = Vec::new();
    let mut name = None;
    let mut publisher = None;
    let mut license = None;
    let mut key = None;
    let mut output = None;
    let mut confined = false;
    let mut everyone = false;
    let mut yes = false;
    let mut data = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Long(l) if l == "for-everyone" => everyone = true,
            Arg::Long(l) if l == "yes" => yes = true,
            Arg::Short('y') => yes = true,
            Arg::Long(l) if l == "keep-data" && data.is_none() => data = Some(false),
            Arg::Long(l) if l == "purge" && data.is_none() => data = Some(true),
            Arg::Long(l) if l == "name" => name = Some(args.text()?),
            Arg::Long(l) if l == "publisher" => publisher = Some(args.text()?),
            Arg::Long(l) if l == "license" => license = Some(args.text()?),
            Arg::Long(l) if l == "key" => key = Some(args.text()?),
            Arg::Long(l) if l == "output" => output = Some(args.text()?),
            Arg::Short('o') => output = Some(args.text()?),
            Arg::Long(l) if l == "confined" => confined = true,
            Arg::Long(l) if l == "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            Arg::Operand(operand) => operands.push(operand.to_string_lossy().into_owned()),
            _ => return Err(usage(USAGE.into())),
        }
    }
    let sub = subcommand.as_deref();
    let new_only = name.is_some() || publisher.is_some() || license.is_some();
    let key_ok = matches!(sub, Some("sign" | "pack" | "verify")) || key.is_none();
    let output_ok = sub == Some("pack") || output.is_none();
    if !key_ok || !output_ok || (confined && (sub != Some("unpack") || !operands.is_empty())) {
        return Err(usage(USAGE.into()));
    }
    let everyone_ok = matches!(sub, Some("install" | "remove")) || !everyone;
    let yes_ok = sub == Some("install") || !yes;
    let data_ok = sub == Some("remove") || data.is_none();
    if !everyone_ok || !yes_ok || !data_ok {
        return Err(usage(USAGE.into()));
    }
    let folder = || operands.first().map_or(".", String::as_str);
    match (sub, operands.as_slice()) {
        (Some("new"), [id, rest @ ..]) if rest.len() <= 1 => {
            new(id, rest.first().map(String::as_str), name, publisher, license)
        }
        _ if new_only => Err(usage(USAGE.into())),
        (Some("check"), [] | [_]) => check(folder()),
        (Some("schema"), []) => {
            println!("{}", bundle::schema());
            Ok(0)
        }
        (Some("upgrade-manifest"), [] | [_]) => upgrade(folder()),
        (Some("key"), []) => show_key(),
        (Some("sign"), [] | [_]) => sign(Path::new(folder()), key.as_deref()).map(|_| 0),
        (Some("pack"), [] | [_]) => pack(Path::new(folder()), key.as_deref(), output.as_deref()),
        (Some("verify"), [] | [_]) => verify(folder(), key.as_deref()),
        (Some("unpack"), []) if confined => Ok(confine::confined()),
        (Some("unpack"), [archive, folder]) => unpack(archive, folder),
        (Some("install"), [archive]) => install(archive, everyone, yes),
        (Some("list"), []) => list(),
        (Some("info"), [id]) => info(id),
        (Some("remove"), [id]) => remove(id, everyone, data),
        (Some("grant"), [id, permission, words @ ..]) => grant(id, permission, words),
        (Some("revoke"), [id, permission]) => revoke(id, permission),
        _ => Err(usage(USAGE.into())),
    }
}

/// `bundle run [--command NAME] ID [ARGUMENT...]`: what follows the ID
/// is the program's, options included.
fn run_main(args: &mut Args) -> Result<i32, Fail> {
    args.stop_at_operand();
    let mut command = None;
    let id = loop {
        match args.next()? {
            Some(Arg::Long(l)) if l == "command" && command.is_none() => {
                command = Some(args.text()?);
            }
            Some(Arg::Long(l)) if l == "help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            Some(Arg::Operand(id)) => break id.to_string_lossy().into_owned(),
            _ => return Err(usage(USAGE.into())),
        }
    };
    run::run(&id, command.as_deref(), args.rest())
}

/// The manifest `what` names: a folder's manifest.json, or a file.
fn manifest_path(what: &str) -> (PathBuf, Option<PathBuf>) {
    let path = Path::new(what);
    if path.is_dir() {
        (path.join(MANIFEST), Some(path.to_path_buf()))
    } else {
        (path.into(), None)
    }
}

/// Reads the manifest at `path`: its text, or why it cannot be read.
fn read(path: &Path) -> Result<String, Fail> {
    if path.extension().is_some_and(|e| e == "dkv") {
        return Err(fail(format!(
            "{}: a packed bundle; check the folder it was made from",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => fail(format!(
            "{}: not there; `bundle new ID` writes a manifest to start from",
            path.display()
        )),
        _ => util::failed(&path.display(), e),
    })?;
    String::from_utf8(bytes).map_err(|_| fail(format!("{}: it is not UTF-8 text", path.display())))
}

/// `bundle new ID [FOLDER]`: a folder for a new bundle, FOLDER or the
/// last part of its ID, with its manifest and a folder for its programs.
fn new(
    id: &str,
    folder: Option<&str>,
    name: Option<String>,
    publisher: Option<String>,
    license: Option<String>,
) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    let short = id.rsplit('.').next().unwrap_or(id);
    let folder = PathBuf::from(folder.unwrap_or(short));
    let name = name.unwrap_or_else(|| short.to_string());
    let publisher = publisher.unwrap_or_else(|| util::user_name(util::own_user()));
    let license = license.unwrap_or_else(|| LICENSE.into());
    bundle::spdx::check(&license).map_err(|why| usage(format!("--license: {why}")))?;
    let text = bundle::template(id, &name, &publisher, &license);
    // What it writes must read: a name or publisher it cannot hold is the
    // command line's mistake.
    if let Err(errors) = bundle::from_json(&text) {
        let why: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        return Err(usage(why.join("; ")));
    }
    fs::create_dir_all(folder.join("bin")).map_err(|e| util::failed(&folder.display(), e))?;
    let path = folder.join(MANIFEST);
    let mut file =
        OpenOptions::new().write(true).create_new(true).mode(0o644).open(&path).map_err(
            |e| match e.kind() {
                io::ErrorKind::AlreadyExists => fail(format!(
                    "{} is there already; check it with `bundle check`",
                    path.display()
                )),
                _ => util::failed(&path.display(), e),
            },
        )?;
    file.write_all(text.as_bytes()).map_err(|e| util::failed(&path.display(), e))?;
    println!("wrote {}", path.display());
    println!(
        "put the program in {}, then check the bundle with `bundle check {}`",
        folder.join("bin").join(short).display(),
        folder.display()
    );
    if license == LICENSE {
        println!(
            "its license is {LICENSE}, your own terms: write an SPDX expression such as MIT in \
             \"license\" to choose another"
        );
    }
    let (key, made) = developer_key()?;
    if made {
        println!(
            "made your developer key, which signs your own builds: {}",
            key_folder()?.join(DEVELOPER_KEY).display()
        );
    }
    let _ = tell_bundled(&key.public());
    Ok(0)
}

/// `bundle check [FOLDER | FILE]`: what is wrong with a manifest, each on
/// a line of its own as `FILE:LINE: why`; in a folder, also each program
/// or icon it names that the folder lacks.
fn check(what: &str) -> Result<i32, Fail> {
    let (path, folder) = manifest_path(what);
    let text = read(&path)?;
    let manifest = match bundle::from_json(&text) {
        Ok(manifest) => manifest,
        Err(errors) => {
            for error in &errors {
                match error.line {
                    Some(line) => eprintln!("{}:{line}: {}", path.display(), error.message),
                    None => eprintln!("{}: {}", path.display(), error.message),
                }
            }
            return Ok(1);
        }
    };
    let missing = folder.as_deref().map_or_default(|folder| missing_files(&manifest, folder));
    for why in &missing {
        eprintln!("{}: {why}", path.display());
    }
    for note in manifest.notes() {
        println!("{}: note: {note}", path.display());
    }
    if !missing.is_empty() {
        return Ok(1);
    }
    println!(
        "{}: ok: {} {}, manifest version {}",
        path.display(),
        manifest.id,
        manifest.version,
        bundle::VERSION
    );
    Ok(0)
}

/// What of the files `manifest` names `folder` lacks: each program, as
/// an executable file, and the icon.
fn missing_files(manifest: &Manifest, folder: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for (name, program) in manifest.programs.iter() {
        for (arch, path) in program.path.iter() {
            match fs::metadata(folder.join(&path.0)) {
                Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => {}
                // A WebAssembly module is not run as a program is.
                Ok(meta) if meta.is_file() && *arch == bundle::Arch::Wasm => {}
                Ok(meta) if meta.is_file() => missing.push(format!(
                    "program {name} for {arch}, {}, may not be run: `chmod +x` it",
                    path.0
                )),
                _ => missing.push(format!("program {name} for {arch} is not at {}", path.0)),
            }
        }
    }
    if let Some(icon) = &manifest.icon
        && !folder.join(&icon.0).is_file()
    {
        missing.push(format!("the icon is not at {}", icon.0));
    }
    missing
}

/// `bundle upgrade-manifest [FOLDER | FILE]`: brings a manifest of an
/// earlier version to this one. Version 1 is the first, so there is
/// nothing earlier to bring up yet: it says so, once the manifest reads.
fn upgrade(what: &str) -> Result<i32, Fail> {
    let (path, _) = manifest_path(what);
    let text = read(&path)?;
    match bundle::from_json(&text) {
        Ok(_) => {
            println!(
                "{}: manifest version {}, the newest this Dukovo reads: nothing to upgrade",
                path.display(),
                bundle::VERSION
            );
            Ok(0)
        }
        Err(errors) => {
            for error in &errors {
                eprintln!("{}: {error}", path.display());
            }
            Ok(1)
        }
    }
}

/// The folder the developer key is in: `$XDG_DATA_HOME/bundle`, or
/// `~/.local/share/bundle`.
fn key_folder() -> Result<PathBuf, Fail> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|d| Path::new(d).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".local/share")))
        .ok_or_else(|| {
            fail("HOME is not set, so there is no folder for the developer key".into())
        })?;
    Ok(data.join("bundle"))
}

/// `count` random bytes.
fn random<const N: usize>() -> Result<[u8; N], Fail> {
    let mut bytes = [0u8; N];
    File::open(RANDOM)
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| util::failed(&RANDOM, e))?;
    Ok(bytes)
}

/// The developer key: a minisign key without a password, which `bundle
/// new` makes the first time and which signs your own builds. Returns it,
/// and whether it was made now.
fn developer_key() -> Result<(SecretKey, bool), Fail> {
    let folder = key_folder()?;
    let path = folder.join(DEVELOPER_KEY);
    match fs::read_to_string(&path) {
        Ok(text) => {
            let key =
                SecretKey::read(&text).map_err(|why| fail(format!("{}: {why}", path.display())))?;
            return Ok((key, false));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(util::failed(&path.display(), e));
        }
        Err(_) => {}
    }
    let key = SecretKey::generate(&random::<40>()?).map_err(fail)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(|e| util::failed(&folder.display(), e))?;
    let text = key.file(
        "Dukovo developer key: it signs your own builds; keep it to yourself",
        &random::<32>()?,
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| util::failed(&path.display(), e))?;
    file.write_all(text.as_bytes()).map_err(|e| util::failed(&path.display(), e))?;
    let public = folder.join(DEVELOPER_PUBLIC);
    fs::write(&public, key.public().file()).map_err(|e| util::failed(&public.display(), e))?;
    Ok((key, true))
}

/// The key to sign with: the file `--key` names, or the developer key.
fn signing_key(file: Option<&str>) -> Result<SecretKey, Fail> {
    match file {
        Some(path) => {
            let text = fs::read_to_string(path).map_err(|e| util::failed(&path, e))?;
            SecretKey::read(&text).map_err(|why| fail(format!("{path}: {why}")))
        }
        None => {
            let (key, made) = developer_key()?;
            if made {
                println!(
                    "made your developer key, which signs your own builds: {}",
                    key_folder()?.join(DEVELOPER_KEY).display()
                );
            }
            Ok(key)
        }
    }
}

/// The public developer key, if this account has one.
fn own_public_key() -> Option<PublicKey> {
    let text = fs::read_to_string(key_folder().ok()?.join(DEVELOPER_PUBLIC)).ok()?;
    PublicKey::read(&text).ok()
}

/// How a key is named to people: the key, and whose it is if it is yours.
fn key_words(key: &PublicKey) -> String {
    if own_public_key().as_ref() == Some(key) {
        format!("your developer key {}", key.encoded())
    } else {
        format!("key {}", key.encoded())
    }
}

/// `bundle key`: the developer key, made if there is none.
fn show_key() -> Result<i32, Fail> {
    let (key, made) = developer_key()?;
    let folder = key_folder()?;
    if made {
        println!("made your developer key: {}", folder.join(DEVELOPER_KEY).display());
    }
    println!("{}", key.public().encoded());
    println!(
        "it signs your own builds; its public half is in {}",
        folder.join(DEVELOPER_PUBLIC).display()
    );
    match tell_bundled(&key.public()) {
        Ok(()) => println!("this computer installs what it signs for you with no warning"),
        Err(why) => eprintln!("bundle: {why}"),
    }
    Ok(0)
}

/// Tells bundled(8) the developer key, which it trusts for this person's
/// own installs (ADR 0223).
fn tell_bundled(key: &PublicKey) -> Result<(), String> {
    let (_, mut input) = ask(&Request::DeveloperKey { key: *key }, None)?;
    match Answer::read(&mut input) {
        Ok(Some(Answer::Ok(_))) => Ok(()),
        Ok(Some(Answer::Error(why))) => Err(why),
        _ => Err("bundled gave no answer".into()),
    }
}

/// Sends `request` to bundled(8), with `archive` for an install; the
/// connection, and a reader of its answers.
fn ask(
    request: &Request,
    archive: Option<&File>,
) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    let stream = UnixStream::connect(protocol::SOCKET).map_err(|e| {
        format!("cannot reach bundled at {}: {}", protocol::SOCKET, util::strip_os_error(&e))
    })?;
    let line = request.line();
    let sent = match archive {
        Some(file) => signin::send_with_fds(&stream, line.as_bytes(), &[file.as_raw_fd()]),
        None => (&stream).write_all(line.as_bytes()),
    };
    sent.map_err(|e| format!("bundled: {e}"))?;
    let input = stream.try_clone().map_err(|e| format!("bundled: {e}"))?;
    Ok((stream, BufReader::new(input)))
}

/// The next answer, or why there is none.
fn answer(input: &mut BufReader<UnixStream>) -> Result<Answer, Fail> {
    match Answer::read(input) {
        Ok(Some(answer)) => Ok(answer),
        Ok(None) => Err(fail("bundled gave no answer".into())),
        Err(e) => Err(fail(format!("bundled: {e}"))),
    }
}

/// Asks `question` on the terminal: whether the line typed starts with
/// one of `yes`'s letters.
fn agree(question: &str, yes: &[char]) -> Result<bool, Fail> {
    print!("{question}");
    io::stdout().flush().map_err(|e| util::failed(&"the output", e))?;
    let mut line = String::new();
    io::stdin().read_line(&mut line).map_err(|e| util::failed(&"the input", e))?;
    Ok(line.trim_start().chars().next().is_some_and(|c| yes.contains(&c.to_ascii_lowercase())))
}

/// `bundle install FILE.dkv`: bundled shows the install sheet, asks, and
/// installs it sealed, for this person or for everyone.
fn install(archive: &str, everyone: bool, yes: bool) -> Result<i32, Fail> {
    let path = Path::new(archive);
    if path.is_dir() {
        return Err(fail(format!(
            "{archive} is a folder: make its .{EXTENSION} file with `bundle pack {archive}`, \
             then install that"
        )));
    }
    let file = File::open(path).map_err(|e| util::failed(&archive, e))?;
    let (stream, mut input) =
        ask(&Request::Install { everyone, yes }, Some(&file)).map_err(fail)?;
    loop {
        match answer(&mut input)? {
            Answer::Sheet(text) => print!("{text}"),
            Answer::Question => {
                let agreed = agree("Install it? [y/N] ", &['y'])?;
                let reply: &[u8] = if agreed { b"yes\n" } else { b"no\n" };
                (&stream).write_all(reply).map_err(|e| fail(format!("bundled: {e}")))?;
            }
            Answer::Ok(what) => {
                println!("{what}");
                return Ok(0);
            }
            Answer::Error(why) => {
                eprintln!("bundle: {archive}: {why}");
                return Ok(1);
            }
            _ => return Err(fail("bundled answered out of turn".into())),
        }
    }
}

/// `bundle list`: the bundles this person can run, theirs and everyone's.
fn list() -> Result<i32, Fail> {
    let (_stream, mut input) = ask(&Request::List, None).map_err(fail)?;
    let mut rows = Vec::new();
    loop {
        match answer(&mut input)? {
            Answer::Listed(l) => rows.push(l),
            Answer::Ok(_) => break,
            Answer::Error(why) => {
                eprintln!("bundle: {why}");
                return Ok(1);
            }
            _ => return Err(fail("bundled answered out of turn".into())),
        }
    }
    if rows.is_empty() {
        println!("no bundles are installed");
        return Ok(0);
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id).then(a.place.cmp(&b.place)));
    let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(2).max(2);
    let version_width = rows.iter().map(|r| r.version.len()).max().unwrap_or(7).max(7);
    println!("{:width$}  {:version_width$}  {:9}  NAME", "ID", "VERSION", "FOR");
    for row in rows {
        let whose = match row.place {
            Place::Everyone => "everyone",
            Place::User(_) => "you",
        };
        println!("{:width$}  {:version_width$}  {whose:9}  {}", row.id, row.version, row.name);
    }
    Ok(0)
}

/// A line of `bundle info` for what the person said of a permission.
fn said_line(grant: &grants::Grant) -> String {
    let when = util::time_text(i64::try_from(grant.since).unwrap_or(0), "%Y-%m-%d", true);
    let what = match grant.said {
        grants::Said::Allow if grant.given.is_empty() => "allowed".to_string(),
        grants::Said::Allow => format!("allowed: {}", grant.given.join(", ")),
        grants::Said::Never => "not allowed".to_string(),
    };
    format!("    {}: {what}, since {when}", grant.permission)
}

/// `bundle info ID`: an installed bundle's sheet, what you said of its
/// permissions, where it is and when it was installed.
fn info(id: &str) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    let (_stream, mut input) = ask(&Request::Info { id: id.into() }, None).map_err(fail)?;
    let mut said = Vec::new();
    loop {
        match answer(&mut input)? {
            Answer::Sheet(text) => print!("{text}"),
            Answer::Granted(grant) => said.push(grant),
            Answer::Ok(what) => {
                if !said.is_empty() {
                    println!("  You said:");
                    said.sort_by(|a, b| a.permission.cmp(&b.permission));
                    for grant in &said {
                        println!("{}", said_line(grant));
                    }
                }
                for part in what.split("; ") {
                    let mut chars = part.chars();
                    let first = chars.next().map(|c| c.to_uppercase().to_string());
                    println!("  {}{}", first.unwrap_or_default(), chars.as_str());
                }
                return Ok(0);
            }
            Answer::Error(why) => {
                eprintln!("bundle: {why}");
                return Ok(1);
            }
            _ => return Err(fail("bundled answered out of turn".into())),
        }
    }
}

/// `bundle remove ID`: removes it, after asking whether to delete its
/// settings and data unless `--keep-data` or `--purge` answers.
fn remove(id: &str, everyone: bool, data: Option<bool>) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    let purge = match data {
        Some(purge) => purge,
        // Removing for everyone leaves each person's data.
        None if everyone => false,
        None if io::stdin().is_terminal() => {
            agree("Also delete its settings and data? [k]eep or [d]elete: ", &['d'])?
        }
        None => {
            return Err(usage(
                "say whether to delete its settings and data: --keep-data or --purge".into(),
            ));
        }
    };
    let (_stream, mut input) =
        ask(&Request::Remove { id: id.into(), everyone, purge }, None).map_err(fail)?;
    match answer(&mut input)? {
        Answer::Ok(what) => {
            println!("{what}");
            if !purge && !everyone {
                println!("its settings and data stay; a later version of it picks them up");
            }
            Ok(0)
        }
        Answer::Error(why) => {
            eprintln!("bundle: {why}");
            Ok(1)
        }
        _ => Err(fail("bundled answered out of turn".into())),
    }
}

/// `bundle grant ID PERMISSION [WORD...]`: gives an app a permission it
/// asks for, as it asks for it, or with less (`documents=read` for its
/// `documents=read-write`), after the trusted prompt's yes on this
/// console (ADR 0224).
fn grant(id: &str, permission: &str, words: &[String]) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    let found = bundle::permissions::find(permission).ok_or_else(|| {
        usage(format!("{permission} is no permission of manifest.json(5)'s list"))
    })?;
    for word in words {
        grants::check_word(found, word).map_err(usage)?;
    }
    let request =
        Request::Grant { id: id.into(), permission: permission.into(), words: words.to_vec() };
    said(&request)
}

/// `bundle revoke ID PERMISSION`: takes a permission away from an app,
/// now and from now on; a run of it that holds it stops.
fn revoke(id: &str, permission: &str) -> Result<i32, Fail> {
    bundle::check_id(id).map_err(usage)?;
    if bundle::permissions::find(permission).is_none() {
        return Err(usage(format!("{permission} is no permission of manifest.json(5)'s list")));
    }
    said(&Request::Revoke { id: id.into(), permission: permission.into() })
}

/// Sends a grant or a revoke, and says what came of it.
fn said(request: &Request) -> Result<i32, Fail> {
    let (_stream, mut input) = ask(request, None).map_err(fail)?;
    match answer(&mut input)? {
        Answer::Ok(what) => {
            println!("{what}");
            Ok(0)
        }
        Answer::Error(why) => {
            eprintln!("bundle: {why}");
            Ok(1)
        }
        _ => Err(fail("bundled answered out of turn".into())),
    }
}

/// The time a signature and an SBOM say they were made: now, or
/// `SOURCE_DATE_EPOCH`, for builds that come out the same each time.
fn build_time() -> i64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or_else(|| (util::now_ns() / 1_000_000_000) as i64)
}

/// `bundle sign [FOLDER]`: checks the bundle, then writes its SBOM if it has
/// none, the list of its files and the signature of the list.
fn sign(folder: &Path, key: Option<&str>) -> Result<Manifest, Fail> {
    let text = read(&folder.join(MANIFEST))?;
    let manifest = signed::parse_manifest(text.as_bytes()).map_err(fail)?;
    let missing = missing_files(&manifest, folder);
    if let Some(why) = missing.first() {
        return Err(fail(format!("{}: {why}", folder.display())));
    }
    let key = signing_key(key)?;
    let time = build_time();
    let created = util::time_text(time, "%Y-%m-%dT%H:%M:%SZ", true);
    let (manifest, files) = signed::sign_folder(folder, &key, time, &created)
        .map_err(|why| fail(format!("{}: {why}", folder.display())))?;
    println!(
        "{}: signed {} {}, {files} files, with {}",
        folder.display(),
        manifest.id,
        manifest.version,
        key_words(&key.public())
    );
    Ok(manifest)
}

/// `bundle pack [FOLDER]`: the folder's `.dkv` file, ID-VERSION.dkv unless
/// `--output` names another. A folder whose signature checks is packed as
/// it is; one that is not signed, or has changed, is signed first, and
/// `--key` signs it again with that key.
fn pack(folder: &Path, key: Option<&str>, output: Option<&str>) -> Result<i32, Fail> {
    if key.is_some() || signed::check_folder(folder).is_err() {
        sign(folder, key)?;
    }
    let checked =
        signed::check_folder(folder).map_err(|why| fail(format!("{}: {why}", folder.display())))?;
    let name = format!("{}-{}.{EXTENSION}", checked.manifest.id, checked.manifest.version);
    let output = PathBuf::from(output.unwrap_or(&name));
    // Written beside, and renamed into place once whole.
    let partial = output.with_extension(format!("{EXTENSION}.partial"));
    let file = File::create(&partial).map_err(|e| util::failed(&partial.display(), e))?;
    let mut out = io::BufWriter::new(file);
    let packed = signed::pack(folder, &mut out).and_then(|checked| {
        out.flush().map_err(|e| e.to_string())?;
        Ok(checked)
    });
    let checked = match packed {
        Ok(checked) => checked,
        Err(why) => {
            let _ = fs::remove_file(&partial);
            return Err(fail(format!("{}: {why}", folder.display())));
        }
    };
    fs::rename(&partial, &output).map_err(|e| util::failed(&output.display(), e))?;
    let size = fs::metadata(&output).map_or(0, |m| m.len());
    println!(
        "wrote {}: {} {}, {} files, {}, signed by {}",
        output.display(),
        checked.manifest.id,
        checked.manifest.version,
        checked.files,
        util::human(size, false),
        key_words(&checked.key)
    );
    Ok(0)
}

/// The public key `--key` gives: the key itself, or a file holding it.
fn public_key(given: &str) -> Result<PublicKey, Fail> {
    if given.starts_with("RW") && !Path::new(given).exists() {
        return PublicKey::decode(given).map_err(|why| usage(format!("--key: {why}")));
    }
    let text = fs::read_to_string(given).map_err(|e| util::failed(&given, e))?;
    PublicKey::read(&text).map_err(|why| fail(format!("{given}: {why}")))
}

/// `bundle verify [FOLDER | FILE.dkv]`: checks a bundle's signature and
/// every file against it, and says whose key signed it; with `--key`,
/// fails unless that key did.
fn verify(what: &str, key: Option<&str>) -> Result<i32, Fail> {
    let wanted = key.map(public_key).transpose()?;
    let path = Path::new(what);
    let checked = if path.is_dir() {
        signed::check_folder(path)
    } else {
        let file = File::open(path).map_err(|e| util::failed(&path.display(), e))?;
        signed::check_archive(&file)
    };
    let checked = match checked {
        Ok(checked) => checked,
        Err(why) => {
            eprintln!("{what}: {why}");
            return Ok(1);
        }
    };
    if let Some(wanted) = wanted
        && wanted != checked.key
    {
        eprintln!("{what}: signed by key {}, not by {}", checked.key.encoded(), wanted.encoded());
        return Ok(1);
    }
    let when = checked
        .signed_at()
        .map_or_default(|t| format!(" on {}", util::time_text(t, "%Y-%m-%d %H:%M UTC", true)));
    println!(
        "{what}: ok: {} {}, {} files, signed by {}{when}",
        checked.manifest.id,
        checked.manifest.version,
        checked.files,
        key_words(&checked.key)
    );
    Ok(0)
}

/// `bundle unpack FILE.dkv FOLDER`: checks the archive and unpacks it into
/// FOLDER, which it makes, in a process that holds nothing else.
fn unpack(archive: &str, folder: &str) -> Result<i32, Fail> {
    let file = File::open(archive).map_err(|e| util::failed(&archive, e))?;
    if Path::new(folder).exists() {
        return Err(fail(format!("{folder} is there already: unpack into a new folder")));
    }
    let report = confine::unpack(&file, Path::new(folder))
        .map_err(|why| fail(format!("{archive}: {why}")))?;
    println!(
        "{folder}: unpacked {} {}, {} files, signed by {}",
        report.id,
        report.version,
        report.files,
        key_words(&report.key)
    );
    Ok(0)
}
