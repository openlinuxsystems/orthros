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

mod base64;
mod confine;
mod minisign;
mod sbom;
mod signed;
mod sums;
mod zip;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use manifest::bundle::{self, Manifest};

use self::minisign::{PublicKey, SecretKey};
use crate::args::{Arg, Args};
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
       bundle unpack FILE.dkv FOLDER";

/// The manifest's name in a bundle's folder.
const MANIFEST: &str = "manifest.json";

/// A packed bundle's extension.
const EXTENSION: &str = "dkv";

/// The developer key's files, in `$XDG_DATA_HOME/bundle`.
const DEVELOPER_KEY: &str = "developer.key";
const DEVELOPER_PUBLIC: &str = "developer.pub";

/// Where random bytes come from.
#[cfg(target_os = "radix")]
const RANDOM: &str = "/devices/urandom";
#[cfg(not(target_os = "radix"))]
const RANDOM: &str = "/dev/urandom";

/// The license `bundle new` writes when none is given: the publisher's
/// own terms, until they choose a license.
const LICENSE: &str = "LicenseRef-Proprietary";

pub fn main(args: &mut Args) -> Result<i32, Fail> {
    let subcommand = args.subcommand().map(|s| s.to_string_lossy().into_owned());
    let mut operands = Vec::new();
    let mut name = None;
    let mut publisher = None;
    let mut license = None;
    let mut key = None;
    let mut output = None;
    let mut confined = false;
    while let Some(arg) = args.next()? {
        match arg {
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
        _ => Err(usage(USAGE.into())),
    }
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
    let (_, made) = developer_key()?;
    if made {
        println!(
            "made your developer key, which signs your own builds: {}",
            key_folder()?.join(DEVELOPER_KEY).display()
        );
    }
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
    Ok(0)
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
