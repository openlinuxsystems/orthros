// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Signing a bundle and checking one (ADR 0222), as a folder or a `.dkv`
//! file.
//!
//! A signed bundle has `files.sha256`, the SHA-256 of each of its other
//! files, and `signature`, minisign's signature of that list, whose first
//! line names the key that made it. Checking one reads the signature, the
//! list and the manifest, and then every file against the list: nothing
//! more, nothing less, and each byte as the list says. The key is the one
//! the signature names: whether to trust it is the installer's question.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use manifest::bundle::{self, Manifest};

use super::minisign::{PublicKey, SecretKey, Signature};
use super::sums::{self, Entry, Hasher, LIST, SIGNATURE};
use super::zip::{self, Archive, Source};
use super::{MANIFEST, sbom};

/// The prefix of the signature's first line, before the key that made it.
pub const SIGNED_BY: &str = "signed by key ";
/// The biggest manifest a bundle carries.
pub const MANIFEST_MAX: u64 = 1 << 20;
/// The biggest list: a million files' lines.
pub const LIST_MAX: u64 = 128 << 20;
/// The biggest signature file.
pub const SIGNATURE_MAX: u64 = 16 << 10;

/// What checking a bundle found: it is signed by `key`, and is `manifest`'s.
#[derive(Debug)]
pub struct Checked {
    pub key: PublicKey,
    /// The signature's trusted comment.
    pub trusted: String,
    pub manifest: Manifest,
    /// How many files its list holds: all but the list and the signature.
    pub files: usize,
    /// The bytes they take.
    pub bytes: u64,
}

/// The trusted comment a bundle's signature carries: minisign's, and the
/// bundle's ID and version.
pub fn trusted_comment(manifest: &Manifest, timestamp: i64) -> String {
    format!(
        "timestamp:{timestamp}\tfile:{LIST}\thashed\tbundle:{} {}",
        manifest.id.0, manifest.version.0
    )
}

impl Checked {
    /// When it was signed, as its trusted comment says.
    pub fn signed_at(&self) -> Option<i64> {
        self.trusted.split('\t').find_map(|field| field.strip_prefix("timestamp:")?.parse().ok())
    }
}

/// Reads a manifest the list names, checking its hash.
fn manifest_of(bytes: &[u8], listed: &[Entry]) -> Result<Manifest, String> {
    let entry = listed
        .iter()
        .find(|e| e.path == MANIFEST)
        .ok_or_else(|| format!("its signed list has no {MANIFEST}"))?;
    if sums::sha256(bytes) != entry.sha256 {
        return Err(format!("{MANIFEST}: changed since it was signed"));
    }
    parse_manifest(bytes)
}

/// Reads a manifest, saying what is wrong with it as `bundle check` does.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| format!("{MANIFEST}: it is not UTF-8 text"))?;
    bundle::from_json(text).map_err(|errors| {
        let shown: Vec<String> = errors
            .iter()
            .take(3)
            .map(|e| match e.line {
                Some(line) => format!("{MANIFEST}:{line}: {}", e.message),
                None => format!("{MANIFEST}: {}", e.message),
            })
            .collect();
        shown.join("; ")
    })
}

/// The key a signature names on its first line, and the signature.
pub fn read_signature(bytes: &[u8]) -> Result<(PublicKey, Signature), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{SIGNATURE}: not text"))?;
    let signature = Signature::read(text).map_err(|why| format!("{SIGNATURE}: {why}"))?;
    let key = signature
        .untrusted
        .strip_prefix(SIGNED_BY)
        .ok_or_else(|| format!("{SIGNATURE}: it does not name its key"))
        .and_then(|key| PublicKey::decode(key).map_err(|why| format!("{SIGNATURE}: {why}")))?;
    Ok((key, signature))
}

/// Checks the signature of `list` and reads the list.
fn check_list(list: &[u8], signature: &[u8]) -> Result<(PublicKey, String, Vec<Entry>), String> {
    let (key, signature) = read_signature(signature)?;
    let trusted = key.verify(list, &signature).map_err(|why| format!("{SIGNATURE}: {why}"))?;
    let text = std::str::from_utf8(list).map_err(|_| format!("{LIST}: not text"))?;
    let entries = sums::read(text)?;
    Ok((key, trusted, entries))
}

/// The files of a bundle's folder, by path, with their sizes: regular
/// files only, and none a bundle may not hold.
pub fn folder_files(folder: &Path) -> Result<Vec<(String, u64, bool)>, String> {
    let mut files = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(prefix) = pending.pop() {
        let dir = folder.join(&prefix);
        let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| format!("{}: a name that is not UTF-8", entry.path().display()))?;
            let path =
                if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
            sums::check_path(&path)?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(format!("{path}: a symbolic link, which a bundle may not hold"));
            } else if kind.is_dir() {
                pending.push(path);
            } else if !kind.is_file() {
                return Err(format!(
                    "{path}: a device file, pipe or socket, which a bundle may not hold"
                ));
            } else if meta.mode() & 0o6000 != 0 {
                return Err(format!(
                    "{path}: it has set-ID bits, which a bundle's files may not have"
                ));
            } else {
                files.push((path, meta.len(), meta.permissions().mode() & 0o111 != 0));
            }
        }
    }
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(files)
}

/// The SHA-256 of a file, read a piece at a time.
fn hash_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Hasher::default();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(hasher.finish()),
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Signs the bundle in `folder` with `key`: writes its SBOM if it has none
/// (made at `created`), its list and its signature. Returns its manifest
/// and how many files the list holds.
pub fn sign_folder(
    folder: &Path,
    key: &SecretKey,
    timestamp: i64,
    created: &str,
) -> Result<(Manifest, usize), String> {
    let text = fs::read(folder.join(MANIFEST))
        .map_err(|e| format!("{}: {e}", folder.join(MANIFEST).display()))?;
    let manifest = parse_manifest(&text)?;
    let sbom_path = folder.join(sbom::SBOM);
    match fs::read_to_string(&sbom_path) {
        Ok(text) if text.len() as u64 > sbom::SBOM_MAX => {
            return Err(format!("{}: bigger than an SBOM may be, 16 MiB", sbom::SBOM));
        }
        Ok(text) => sbom::check(&text)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::write(&sbom_path, sbom::write(&manifest, created))
                .map_err(|e| format!("{}: {e}", sbom_path.display()))?;
        }
        Err(e) => return Err(format!("{}: {e}", sbom_path.display())),
    }
    let mut entries = Vec::new();
    for (path, _, _) in folder_files(folder)? {
        if path == LIST || path == SIGNATURE {
            continue;
        }
        entries.push(Entry { sha256: hash_file(&folder.join(&path))?, path });
    }
    if entries.len() > zip::ENTRIES_MAX - 2 {
        return Err(format!("more files than a bundle holds: {}", zip::ENTRIES_MAX - 2));
    }
    let list = sums::write(&mut entries);
    let signature = key.sign(
        list.as_bytes(),
        &format!("{SIGNED_BY}{}", key.public().encoded()),
        &trusted_comment(&manifest, timestamp),
    );
    for (name, text) in [(LIST, list.as_str()), (SIGNATURE, &signature.file())] {
        let path = folder.join(name);
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok((manifest, entries.len()))
}

/// Checks the bundle in `folder`: its signature, its list against its
/// files, and its manifest.
pub fn check_folder(folder: &Path) -> Result<Checked, String> {
    let read = |name: &str, max: u64| -> Result<Vec<u8>, String> {
        let path = folder.join(name);
        let meta = fs::symlink_metadata(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound if name == SIGNATURE => {
                "it is not signed: sign it with `bundle sign`".to_string()
            }
            _ => format!("{}: {e}", path.display()),
        })?;
        if !meta.is_file() || meta.len() > max {
            return Err(format!("{name}: not a file of at most {max} bytes"));
        }
        fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let signature = read(SIGNATURE, SIGNATURE_MAX)?;
    let list = read(LIST, LIST_MAX)?;
    let (key, trusted, listed) = check_list(&list, &signature)?;
    let manifest = manifest_of(&read(MANIFEST, MANIFEST_MAX)?, &listed)?;
    let files = folder_files(folder)?;
    let mut bytes = 0;
    let mut unlisted = Vec::new();
    let mut wanted: BTreeSet<&str> = listed.iter().map(|e| e.path.as_str()).collect();
    for (path, size, _) in &files {
        bytes += size;
        if path == LIST || path == SIGNATURE {
            continue;
        }
        if !wanted.remove(path.as_str()) {
            unlisted.push(path.as_str());
        }
    }
    if let Some(path) = unlisted.first() {
        return Err(format!("{path}: not in its signed list: it was added after signing"));
    }
    if let Some(path) = wanted.first() {
        return Err(format!("{path}: in its signed list, but not there"));
    }
    for entry in &listed {
        if hash_file(&folder.join(&entry.path))? != entry.sha256 {
            return Err(format!("{}: changed since it was signed", entry.path));
        }
    }
    Ok(Checked { key, trusted, manifest, files: listed.len(), bytes })
}

/// What takes a file's bytes as they come.
pub type Sink<'a> = &'a mut dyn FnMut(&[u8]) -> Result<(), String>;
/// What hands a file's bytes to a sink.
pub type Fill<'a> = &'a mut dyn FnMut(Sink<'_>) -> Result<(), String>;

/// Where unpacked files go: a folder, through whatever reaches it.
pub trait Target {
    /// Makes a folder; its parent is there.
    fn make_folder(&mut self, path: &str) -> Result<(), String>;
    /// Makes a file, which must not be there, and writes it as `fill`
    /// hands it bytes.
    fn write_file(&mut self, path: &str, executable: bool, fill: Fill<'_>) -> Result<(), String>;
    /// The bytes its filesystem has free, if it says.
    fn free(&self) -> Option<u64>;
}

/// A folder this program reaches by path, as tests and the build machine
/// do; on Dukovo the unpacker reaches its folder by capability alone.
#[cfg(any(test, not(target_os = "radix")))]
pub struct FolderTarget<'a>(pub &'a Path);

#[cfg(any(test, not(target_os = "radix")))]
impl Target for FolderTarget<'_> {
    fn make_folder(&mut self, path: &str) -> Result<(), String> {
        fs::create_dir(self.0.join(path)).map_err(|e| format!("{path}: {e}"))
    }

    fn write_file(&mut self, path: &str, executable: bool, fill: Fill<'_>) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o755 } else { 0o644 })
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.0.join(path))
            .map_err(|e| format!("{path}: {e}"))?;
        fill(&mut |bytes| file.write_all(bytes).map_err(|e| format!("{path}: {e}")))
    }

    fn free(&self) -> Option<u64> {
        None
    }
}

/// Checks a `.dkv` archive without unpacking it to a folder: the zip, the
/// signature, the list against the archive's files, the manifest, and
/// every file's bytes against the list.
pub fn check_archive<S: Source + ?Sized>(source: &S) -> Result<Checked, String> {
    unpack(source, None)
}

/// Checks a `.dkv` archive and unpacks it into `target`, which is empty.
/// A program the manifest names is executable, and so is a file the
/// archive marks so; nothing else is. Fails at the first problem, leaving
/// what it unpacked so far for the caller to remove.
pub fn unpack<S: Source + ?Sized>(
    source: &S,
    mut target: Option<&mut dyn Target>,
) -> Result<Checked, String> {
    let archive = Archive::read(source)?;
    let small = |name: &str, max: u64, missing: &str| -> Result<Vec<u8>, String> {
        let entry = archive.file(name).ok_or_else(|| missing.to_string())?;
        archive.read_small(entry, max)
    };
    let signature = small(SIGNATURE, SIGNATURE_MAX, "it is not signed: it has no signature")?;
    let list = small(LIST, LIST_MAX, &format!("it has no {LIST}, the list its signature signs"))?;
    let (key, trusted, listed) = check_list(&list, &signature)?;
    let manifest =
        manifest_of(&small(MANIFEST, MANIFEST_MAX, &format!("it has no {MANIFEST}"))?, &listed)?;
    // The archive holds the list's files, and only those, with the list and
    // the signature; its folders need not be named.
    let mut wanted: BTreeSet<&str> = listed.iter().map(|e| e.path.as_str()).collect();
    for entry in archive.entries.iter().filter(|e| !e.folder) {
        if entry.path != LIST && entry.path != SIGNATURE && !wanted.remove(entry.path.as_str()) {
            return Err(format!(
                "{}: not in its signed list: it was added after signing",
                entry.path
            ));
        }
    }
    if let Some(path) = wanted.first() {
        return Err(format!("{path}: in its signed list, but not in the archive"));
    }
    let bytes: u64 = archive.entries.iter().map(|e| e.size).sum();
    if let Some(free) = target.as_ref().and_then(|t| t.free())
        && bytes > free
    {
        return Err(format!("it unpacks to {bytes} bytes, and the disk has {free} free"));
    }
    let programs: BTreeSet<&str> = manifest
        .programs
        .iter()
        .flat_map(|(_, program)| program.path.iter().map(|(_, path)| path.0.as_str()))
        .collect();
    let mut folders: BTreeSet<String> = BTreeSet::new();
    let mut files = 0;
    for listed in &listed {
        let Some(entry) = archive.file(&listed.path) else { continue };
        let mut hasher = Hasher::default();
        match target.as_deref_mut() {
            Some(target) => {
                let mut parent = String::new();
                for part in
                    listed.path.split('/').rev().skip(1).collect::<Vec<_>>().into_iter().rev()
                {
                    if !parent.is_empty() {
                        parent.push('/');
                    }
                    parent.push_str(part);
                    if folders.insert(parent.clone()) {
                        target.make_folder(&parent)?;
                    }
                }
                let executable = entry.executable || programs.contains(listed.path.as_str());
                target.write_file(&listed.path, executable, &mut |write| {
                    archive.unpack(entry, &mut |chunk| {
                        hasher.update(chunk);
                        write(chunk)
                    })
                })?;
            }
            None => archive.unpack(entry, &mut |chunk| {
                hasher.update(chunk);
                Ok(())
            })?,
        }
        if hasher.finish() != listed.sha256 {
            return Err(format!("{}: changed since it was signed", listed.path));
        }
        files += 1;
    }
    if let Some(target) = target {
        for (name, bytes) in [(LIST, &list), (SIGNATURE, &signature)] {
            target.write_file(name, false, &mut |write| write(bytes))?;
        }
    }
    Ok(Checked { key, trusted, manifest, files, bytes })
}

/// Packs the signed bundle in `folder` into a `.dkv` archive written to
/// `out`: the manifest, the signature and the list first, then the rest
/// in the list's order. Checks the folder first.
pub fn pack(folder: &Path, out: impl Write) -> Result<Checked, String> {
    let checked = check_folder(folder)?;
    let list = fs::read_to_string(folder.join(LIST)).map_err(|e| format!("{LIST}: {e}"))?;
    let listed = sums::read(&list)?;
    let files = folder_files(folder)?;
    let executable = |path: &str| files.iter().any(|(p, _, x)| p == path && *x);
    let mut writer = zip::Writer::new(out);
    let mut order = vec![MANIFEST.to_string(), SIGNATURE.into(), LIST.into()];
    order.extend(listed.into_iter().map(|e| e.path).filter(|p| p != MANIFEST));
    for path in &order {
        let data = fs::read(folder.join(path)).map_err(|e| format!("{path}: {e}"))?;
        writer
            .add(&zip::Input { path, executable: executable(path), data: &data })
            .map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SecretKey {
        let mut random = [seed; 40];
        random[0] = seed.wrapping_add(1);
        SecretKey::generate(&random).unwrap()
    }

    /// A bundle's folder in a new temporary folder, with one program.
    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bundle-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("hello/bin")).unwrap();
        let manifest = bundle::template("org.example.hello", "Hello", "Example", "MIT");
        fs::write(dir.join("hello/manifest.json"), manifest).unwrap();
        fs::write(dir.join("hello/bin/hello"), b"#!/bin/sh\necho hello\n").unwrap();
        fs::set_permissions(dir.join("hello/bin/hello"), fs::Permissions::from_mode(0o755))
            .unwrap();
        fs::create_dir_all(dir.join("hello/share/doc")).unwrap();
        fs::write(dir.join("hello/share/doc/README"), "Hello.\n".repeat(100)).unwrap();
        dir
    }

    #[test]
    fn a_signed_folder_checks_and_packs_to_an_archive_that_checks() {
        let dir = folder("pack");
        let bundle = dir.join("hello");
        let key = key(1);
        assert!(check_folder(&bundle).unwrap_err().contains("not signed"));
        let (manifest, files) =
            sign_folder(&bundle, &key, 1_761_912_000, "2026-10-31T12:00:00Z").unwrap();
        assert_eq!(manifest.id.0, "org.example.hello");
        // The manifest, the program, the README and the SBOM it wrote.
        assert_eq!(files, 4);
        assert!(fs::read_to_string(bundle.join(sbom::SBOM)).unwrap().starts_with("SPDXVersion"));
        let checked = check_folder(&bundle).unwrap();
        assert_eq!(checked.key, key.public());
        assert!(checked.trusted.ends_with("\tbundle:org.example.hello 0.1.0"));
        assert_eq!(checked.signed_at(), Some(1_761_912_000));
        let mut bytes = Vec::new();
        pack(&bundle, &mut bytes).unwrap();
        let checked = check_archive(&bytes[..]).unwrap();
        assert_eq!(checked.key, key.public());
        assert_eq!(checked.files, 4);
        // Unpacked, it is the folder again, and checks as one.
        let out = dir.join("out");
        fs::create_dir(&out).unwrap();
        unpack(&bytes[..], Some(&mut FolderTarget(&out))).unwrap();
        check_folder(&out).unwrap();
        let mode = fs::metadata(out.join("bin/hello")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let mode = fs::metadata(out.join("share/doc/README")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        // The archive is the same each time.
        let mut again = Vec::new();
        pack(&bundle, &mut again).unwrap();
        assert_eq!(again, bytes);
        // minisign reads the signature, as `minisign -Vm files.sha256 -x
        // signature -P KEY` would.
        let signature = fs::read_to_string(bundle.join(SIGNATURE)).unwrap();
        assert!(signature.starts_with(&format!("untrusted comment: {SIGNED_BY}RW")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_bundle_does_not_check() {
        let dir = folder("changed");
        let bundle = dir.join("hello");
        sign_folder(&bundle, &key(2), 0, "2026-10-31T12:00:00Z").unwrap();
        fs::write(bundle.join("share/doc/README"), "Changed.\n").unwrap();
        assert!(check_folder(&bundle).unwrap_err().contains("changed since it was signed"));
        sign_folder(&bundle, &key(2), 0, "2026-10-31T12:00:00Z").unwrap();
        fs::write(bundle.join("extra"), "x").unwrap();
        assert!(check_folder(&bundle).unwrap_err().contains("added after signing"));
        fs::remove_file(bundle.join("extra")).unwrap();
        fs::remove_file(bundle.join("share/doc/README")).unwrap();
        assert!(check_folder(&bundle).unwrap_err().contains("not there"));
        sign_folder(&bundle, &key(2), 0, "2026-10-31T12:00:00Z").unwrap();
        // A signature by another key over the same list is that key's.
        let list = fs::read(bundle.join(LIST)).unwrap();
        let other = key(3);
        let forged = key(2).sign(&list, &format!("{SIGNED_BY}{}", other.public().encoded()), "x");
        fs::write(bundle.join(SIGNATURE), forged.file()).unwrap();
        assert!(check_folder(&bundle).unwrap_err().contains("signed by key"));
        std::os::unix::fs::symlink("/etc/passwd", bundle.join("link")).unwrap();
        assert!(sign_folder(&bundle, &key(2), 0, "x").unwrap_err().contains("symbolic link"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_archive_does_not_check() {
        let dir = folder("archive");
        let bundle = dir.join("hello");
        sign_folder(&bundle, &key(4), 0, "2026-10-31T12:00:00Z").unwrap();
        let mut bytes = Vec::new();
        pack(&bundle, &mut bytes).unwrap();
        // Every byte changed in turn: none checks with other content and
        // none panics.
        let original = check_archive(&bytes[..]).unwrap();
        for i in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[i] ^= 0x20;
            if let Ok(checked) = check_archive(&damaged[..]) {
                assert_eq!(checked.key, original.key, "byte {i}");
                assert_eq!(checked.bytes, original.bytes, "byte {i}");
            }
        }
        // A file added to the archive, unsigned.
        let mut writer = zip::Writer::new(Vec::new());
        let archive = Archive::read(&bytes[..]).unwrap();
        for entry in &archive.entries {
            let data = archive.read_small(entry, u64::MAX).unwrap();
            writer
                .add(&zip::Input { path: &entry.path, executable: entry.executable, data: &data })
                .unwrap();
        }
        writer.add(&zip::Input { path: "bin/extra", executable: true, data: b"x" }).unwrap();
        let added = writer.finish().unwrap();
        assert!(check_archive(&added[..]).unwrap_err().contains("added after signing"));
        let _ = fs::remove_dir_all(&dir);
    }
}
