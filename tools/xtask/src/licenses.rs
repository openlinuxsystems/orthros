// SPDX-FileCopyrightText: 2026 The OrthrOS contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The license manifest and notices that every image carries.
//!
//! `ob image` lists every component that ends up in the image, checks each
//! license against the allow-list of the component's area, and writes an
//! SPDX 2.3 document, the license texts and each component's own notice
//! files to `/system/share/licenses`. See
//! docs/adr/0021-license-manifest-in-every-image.md.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, VERSION, kernel};

/// Where the manifest and notices go, relative to the image root.
pub const DIR: &str = "system/share/licenses";

/// File name of the SPDX document, in [`DIR`] and next to the image.
pub const MANIFEST: &str = "orthros.spdx";

/// The allow-list of the kernel area. It is also the cargo-deny policy of the
/// kernel workspace, so CI and `ob image` enforce the same list.
pub const KERNEL_POLICY: &str = "kernel/deny.toml";

/// Licenses a boot loader may use: it is a separate program that only loads
/// the kernel, so the GPL does not reach OrthrOS code (ADR 0021).
const BOOTLOADER_ALLOW: &[&str] = &["GPL-3.0-or-later"];

/// Licenses of closed firmware, which only a local build adds (ADR 0007).
const FIRMWARE_ALLOW: &[&str] = &["LicenseRef-Intel-Microcode"];

/// Names of the files in a source tree that hold license terms or notices.
const NOTICE_PREFIXES: &[&str] = &["LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE"];

/// Where in rust-src each crate of `-Zbuild-std` lives.
const SYSROOT_CRATES: &[(&str, &str)] = &[
    ("core", "core"),
    ("alloc", "alloc"),
    ("compiler_builtins", "compiler-builtins/compiler-builtins"),
];

/// Which allow-list a component is checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// Linked into the kernel (and, later, /system Rust code): permissive only.
    Kernel,
    /// A separate boot loader program.
    Bootloader,
    /// Closed firmware loaded by the kernel.
    Firmware,
}

impl Area {
    fn describe(self) -> &'static str {
        match self {
            Area::Kernel => "the kernel allow-list in kernel/deny.toml",
            Area::Bootloader => "the boot loader allow-list in tools/xtask/src/licenses.rs",
            Area::Firmware => "the firmware allow-list in tools/xtask/src/licenses.rs",
        }
    }
}

/// One component of an image.
#[derive(Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// SPDX license expression.
    pub license: String,
    pub area: Area,
    /// SPDX download location, or `NOASSERTION`.
    pub download: String,
    /// SHA-256 of the source archive, when there is one (crates.io).
    pub sha256: Option<String>,
    /// License and notice files from the component's source.
    pub notices: Vec<PathBuf>,
    pub comment: String,
}

impl Package {
    fn spdx_id(&self) -> String {
        spdx_id(&format!("{}-{}", self.name, self.version))
    }

    fn notice_dir(&self) -> String {
        format!("notices/{}-{}", self.name, self.version)
    }
}

/// What besides the kernel's crates goes into an image.
pub struct Extras<'a> {
    /// The grub-mkrescue that assembles the image.
    pub mkrescue: &'a Path,
    /// Whether the image carries Intel microcode.
    pub microcode: bool,
}

/// Lists the image's components, checks their licenses, and writes the
/// manifest and notices into `staging`. Returns the manifest's text.
pub fn install(ctx: &Ctx, staging: &Path, extras: &Extras<'_>) -> Result<String> {
    let mut packages = kernel_crates(ctx)?;
    packages.extend(sysroot_crates(ctx)?);
    packages.push(grub(ctx, extras.mkrescue)?);
    if extras.microcode {
        packages.push(microcode());
    }
    let kernel_allow = kernel_allow_list(ctx)?;
    check(&packages, &kernel_allow)?;
    let ids = license_ids(&packages)?;

    let dir = staging.join(DIR);
    process::create_dir(&dir.join("texts"))?;
    for id in &ids {
        process::copy(&text_source(&ctx.root, id), &dir.join(format!("texts/{id}.txt")))?;
    }
    for package in &packages {
        for notice in &package.notices {
            let name = notice.file_name().expect("a notice is a file");
            process::copy(notice, &dir.join(package.notice_dir()).join(name))?;
        }
    }
    let document = Document {
        name: format!("orthros-{VERSION}-x86_64"),
        commit: git_commit(ctx),
        created: crate::image::source_date_epoch(ctx).and_then(|e| e.parse().ok()).unwrap_or(0),
    };
    let manifest = document.render(&packages, &ids, &ctx.root)?;
    write(&dir.join(MANIFEST), &manifest)?;
    write(&dir.join("README"), &readme(&packages))?;
    step(&format!(
        "Wrote the license manifest of {} components to /{DIR}/{MANIFEST}",
        packages.len()
    ));
    Ok(manifest)
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).context(|| format!("writing {}", path.display()))
}

/// The crates linked into the kernel, from `cargo tree`. Build scripts and
/// dev-dependencies never reach the image, so only normal edges count.
fn kernel_crates(ctx: &Ctx) -> Result<Vec<Package>> {
    let mut cmd = process::cargo(ctx);
    cmd.args(["tree", "--manifest-path", "kernel/Cargo.toml", "-p", "radix", "--target"])
        .arg(ctx.root.join(kernel::TARGET_SPEC))
        .args(["-Zjson-target-spec", "-e", "normal", "--prefix", "none"])
        .args(["--format", "{p}|{l}"]);
    let output = process::output(ctx, &mut cmd)?;
    let lock = read_lock(&ctx.root.join("kernel/Cargo.lock"))?;
    let commit = git_commit(ctx);
    let mut seen = BTreeSet::new();
    let mut packages = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(entry) = parse_tree_line(line) else { continue };
        if !seen.insert((entry.name.clone(), entry.version.clone())) {
            continue;
        }
        let mut package = Package {
            license: entry.license,
            area: Area::Kernel,
            download: "NOASSERTION".into(),
            sha256: None,
            notices: Vec::new(),
            comment: String::new(),
            name: entry.name,
            version: entry.version,
        };
        match entry.source {
            Source::Path(path) => {
                package.download =
                    format!("git+https://github.com/openlinuxsystems/orthros@{commit}");
                package.comment = "Part of OrthrOS.".into();
                package.notices = declared_notices(&path)?;
                package.notices.extend(notice_files(&path));
            }
            Source::Registry => {
                let key = (package.name.clone(), package.version.clone());
                package.sha256 = lock.get(&key).cloned();
                package.download = format!(
                    "https://crates.io/api/v1/crates/{}/{}/download",
                    package.name, package.version
                );
                package.comment = "A crate from crates.io.".into();
                if let Some(dir) = registry_dir(&package.name, &package.version) {
                    package.notices = notice_files(&dir);
                }
            }
            Source::Other(source) => {
                crate::bail!(
                    "the kernel uses {} {} from {source}; only crates.io and the \
                     repository are allowed (deny.toml [sources])",
                    package.name,
                    package.version
                );
            }
        }
        packages.push(package);
    }
    if packages.is_empty() {
        crate::bail!("`cargo tree` listed no kernel crates");
    }
    Ok(packages)
}

/// Where a crate came from, as `cargo tree` prints it.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Path(PathBuf),
    Registry,
    Other(String),
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    name: String,
    version: String,
    source: Source,
    license: String,
}

/// Parses `name vX.Y.Z (source)|license`, with an optional ` (*)` that
/// marks a crate printed before.
fn parse_tree_line(line: &str) -> Option<TreeEntry> {
    let (package, license) = line.split_once('|')?;
    let license = license.trim().trim_end_matches("(*)").trim().to_string();
    let mut words = package.trim().splitn(3, ' ');
    let name = words.next()?.to_string();
    let version = words.next()?.strip_prefix('v')?.to_string();
    let source = match words.next().map(|s| s.trim().trim_start_matches('(').trim_end_matches(')'))
    {
        None | Some("") => Source::Registry,
        Some(s) if s.starts_with('/') => Source::Path(PathBuf::from(s)),
        Some(s) => Source::Other(s.to_string()),
    };
    Some(TreeEntry { name, version, source, license })
}

/// `[package.metadata.orthros] notices`: notice files a crate carries
/// outside its top directory, such as the font license of radix-fbcon.
fn declared_notices(dir: &Path) -> Result<Vec<PathBuf>> {
    let path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).context(|| format!("reading {}", path.display()))?;
    let manifest: toml::Value =
        toml::from_str(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
    let Some(list) = manifest
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("orthros"))
        .and_then(|o| o.get("notices"))
    else {
        return Ok(Vec::new());
    };
    let mut notices = Vec::new();
    for item in list.as_array().into_iter().flatten() {
        let Some(name) = item.as_str() else {
            crate::bail!("{}: package.metadata.orthros.notices must hold paths", path.display());
        };
        let notice = dir.join(name);
        if !notice.is_file() {
            crate::bail!("{} names notice {name}, which does not exist", path.display());
        }
        notices.push(notice);
    }
    Ok(notices)
}

/// The license and notice files at the top of a source tree, sorted.
fn notice_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_notice(path))
        .collect();
    files.sort();
    files
}

fn is_notice(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let upper = name.to_ascii_uppercase();
    NOTICE_PREFIXES.iter().any(|prefix| upper.starts_with(prefix))
}

/// The unpacked source of a crates.io crate in Cargo's cache.
fn registry_dir(name: &str, version: &str) -> Option<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cargo")))?;
    let src = home.join("registry/src");
    std::fs::read_dir(src)
        .ok()?
        .flatten()
        .map(|index| index.path().join(format!("{name}-{version}")))
        .find(|dir| dir.is_dir())
}

/// (name, version) to checksum, from a Cargo.lock.
fn read_lock(path: &Path) -> Result<std::collections::BTreeMap<(String, String), String>> {
    let text = std::fs::read_to_string(path).context(|| format!("reading {}", path.display()))?;
    let lock: toml::Value =
        toml::from_str(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
    let mut sums = std::collections::BTreeMap::new();
    for package in lock.get("package").and_then(|p| p.as_array()).into_iter().flatten() {
        let field = |key: &str| package.get(key).and_then(|v| v.as_str()).map(str::to_string);
        if let (Some(name), Some(version), Some(sum)) =
            (field("name"), field("version"), field("checksum"))
        {
            sums.insert((name, version), sum);
        }
    }
    Ok(sums)
}

/// The standard library crates that `-Zbuild-std` compiles into the kernel.
fn sysroot_crates(ctx: &Ctx) -> Result<Vec<Package>> {
    let rustc = rustc_info(ctx)?;
    let library = rustc.sysroot.join("lib/rustlib/src/rust/library");
    let mut packages = Vec::new();
    for name in build_std_crates(&kernel::target_args(ctx)) {
        let Some((_, path)) = SYSROOT_CRATES.iter().find(|(crate_name, _)| *crate_name == name)
        else {
            crate::bail!(
                "the kernel builds `{name}` from rust-src, but tools/xtask/src/licenses.rs \
                 does not know where it lives (SYSROOT_CRATES)"
            );
        };
        let dir = library.join(path);
        let manifest_path = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest_path)
            .context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: toml::Value = toml::from_str(&text)
            .map_err(|err| Error::new(format!("{}: {err}", manifest_path.display())))?;
        let field =
            |key: &str| manifest.get("package").and_then(|p| p.get(key)).and_then(|v| v.as_str());
        let version = match field("version") {
            Some(v) if v != "0.0.0" => v.to_string(),
            _ => rustc.release.clone(),
        };
        // Notices sit in the crate or in its repository's top directory
        // (compiler-builtins/LICENSE.txt), never above `library`.
        let mut notices = Vec::new();
        let mut current = dir.as_path();
        while notices.is_empty() && current.starts_with(&library) && current != library {
            notices = notice_files(current);
            current = current.parent().expect("inside the library directory");
        }
        packages.push(Package {
            name: name.to_string(),
            version,
            license: field("license").unwrap_or_default().to_string(),
            area: Area::Kernel,
            download: format!("git+https://github.com/rust-lang/rust@{}", rustc.commit),
            sha256: None,
            notices,
            comment: format!(
                "Rust standard library crate compiled into the kernel with -Zbuild-std, \
                 from rustc {}.",
                rustc.release
            ),
        });
    }
    Ok(packages)
}

/// The crates named by `-Zbuild-std=...` in `args`.
fn build_std_crates(args: &[String]) -> Vec<&str> {
    args.iter()
        .filter_map(|arg| arg.strip_prefix("-Zbuild-std="))
        .flat_map(|list| list.split(','))
        .filter(|name| !name.is_empty())
        .collect()
}

struct Rustc {
    sysroot: PathBuf,
    release: String,
    commit: String,
}

fn rustc_info(ctx: &Ctx) -> Result<Rustc> {
    let run = |args: &[&str]| -> Result<String> {
        let mut cmd = Command::new("rustc");
        cmd.args(args).current_dir(&ctx.root);
        let output = process::output(ctx, &mut cmd)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let sysroot = PathBuf::from(run(&["--print", "sysroot"])?.trim());
    let verbose = run(&["-vV"])?;
    let field = |key: &str| {
        verbose
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    Ok(Rustc { sysroot, release: field("release:"), commit: field("commit-hash:") })
}

/// GRUB, which grub-mkrescue copies from the build host into every image.
fn grub(ctx: &Ctx, mkrescue: &Path) -> Result<Package> {
    let output = process::output(ctx, Command::new(mkrescue).arg("--version"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = grub_version(&text).unwrap_or("NOASSERTION").to_string();
    Ok(Package {
        name: "grub".into(),
        version: version.clone(),
        license: "GPL-3.0-or-later".into(),
        area: Area::Bootloader,
        download: "NOASSERTION".into(),
        sha256: None,
        notices: Vec::new(),
        comment: format!(
            "The GRUB {version} boot loader and its modules, copied from the build \
             host by grub-mkrescue. GRUB is a separate program that loads the kernel. \
             Its corresponding source is the build host distribution's source package \
             for this version; see https://www.gnu.org/software/grub/."
        ),
    })
}

/// The version in `grub-mkrescue (GRUB) 2.12-1ubuntu7.3`.
fn grub_version(text: &str) -> Option<&str> {
    text.lines().next()?.split_whitespace().last()
}

fn microcode() -> Package {
    Package {
        name: "intel-microcode".into(),
        version: "NOASSERTION".into(),
        license: "LicenseRef-Intel-Microcode".into(),
        area: Area::Firmware,
        download: "https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files".into(),
        sha256: None,
        notices: Vec::new(),
        comment: "Closed Intel CPU microcode, added to this local build with \
                  image.microcode or --microcode. It may be redistributed only unmodified; \
                  release images never carry it (ADR 0007)."
            .into(),
    }
}

/// `[licenses] allow` of kernel/deny.toml.
pub fn kernel_allow_list(ctx: &Ctx) -> Result<Vec<String>> {
    let path = ctx.root.join(KERNEL_POLICY);
    let text = std::fs::read_to_string(&path).context(|| format!("reading {}", path.display()))?;
    parse_allow_list(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))
}

fn parse_allow_list(text: &str) -> Result<Vec<String>, String> {
    let policy: toml::Value = toml::from_str(text).map_err(|err| err.to_string())?;
    let allow = policy
        .get("licenses")
        .and_then(|l| l.get("allow"))
        .and_then(|a| a.as_array())
        .ok_or("no [licenses] allow list")?;
    allow
        .iter()
        .map(|item| item.as_str().map(str::to_string).ok_or("allow holds a non-string".into()))
        .collect()
}

/// Fails when a component has no license or one outside its area.
fn check(packages: &[Package], kernel_allow: &[String]) -> Result<()> {
    let mut problems = Vec::new();
    for package in packages {
        let allowed: Vec<String> = match package.area {
            Area::Kernel => kernel_allow.to_vec(),
            Area::Bootloader => BOOTLOADER_ALLOW.iter().map(|s| s.to_string()).collect(),
            Area::Firmware => FIRMWARE_ALLOW.iter().map(|s| s.to_string()).collect(),
        };
        let what = format!("{} {}", package.name, package.version);
        if package.license.is_empty() {
            problems.push(format!("{what} declares no SPDX license"));
            continue;
        }
        match Expr::parse(&package.license) {
            Err(err) => problems.push(format!("{what}: license `{}`: {err}", package.license)),
            Ok(expr) if !expr.satisfied_by(&allowed) => problems.push(format!(
                "{what}: license `{}` is outside {}",
                package.license,
                package.area.describe()
            )),
            Ok(_) => {}
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!("license check failed:\n  {}", problems.join("\n  ")))
        .hint("see docs/adr/0021-license-manifest-in-every-image.md for the policy"))
}

/// Every license and exception named in the manifest, each of which needs
/// its text in the image.
fn license_ids(packages: &[Package]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for package in packages {
        Expr::parse(&package.license)
            .map_err(|err| Error::new(format!("{}: {err}", package.name)))?
            .collect_ids(&mut ids);
    }
    Ok(ids)
}

/// Where the text of a license or exception is kept in the repository.
/// Licenses that files in the repository use are in LICENSES/, as the REUSE
/// specification asks; every other text is in image/licenses as
/// `LICENSE-<id>.txt`. Each text is kept once, verbatim from the SPDX
/// license list (ADR 0026).
pub fn text_source(root: &Path, id: &str) -> PathBuf {
    let reuse = root.join("LICENSES").join(format!("{id}.txt"));
    if reuse.is_file() {
        reuse
    } else {
        root.join("image/licenses").join(format!("LICENSE-{id}.txt"))
    }
}

/// A parsed SPDX license expression.
#[derive(Debug, PartialEq, Eq)]
enum Expr {
    /// A license, or `<license> WITH <exception>`.
    License {
        id: String,
        exception: Option<String>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    /// Parses an expression. The `/` that old crates use for OR is accepted.
    fn parse(text: &str) -> Result<Expr, String> {
        let spaced = text.replace('(', " ( ").replace(')', " ) ").replace('/', " OR ");
        let tokens: Vec<&str> = spaced.split_whitespace().collect();
        let mut pos = 0;
        let expr = Self::parse_or(&tokens, &mut pos)?;
        if pos != tokens.len() {
            return Err(format!("unexpected `{}`", tokens[pos]));
        }
        Ok(expr)
    }

    fn parse_or(tokens: &[&str], pos: &mut usize) -> Result<Expr, String> {
        let mut left = Self::parse_and(tokens, pos)?;
        while tokens.get(*pos) == Some(&"OR") {
            *pos += 1;
            left = Expr::Or(Box::new(left), Box::new(Self::parse_and(tokens, pos)?));
        }
        Ok(left)
    }

    fn parse_and(tokens: &[&str], pos: &mut usize) -> Result<Expr, String> {
        let mut left = Self::parse_atom(tokens, pos)?;
        while tokens.get(*pos) == Some(&"AND") {
            *pos += 1;
            left = Expr::And(Box::new(left), Box::new(Self::parse_atom(tokens, pos)?));
        }
        Ok(left)
    }

    fn parse_atom(tokens: &[&str], pos: &mut usize) -> Result<Expr, String> {
        let token = *tokens.get(*pos).ok_or("the expression ends early")?;
        *pos += 1;
        if token == "(" {
            let inner = Self::parse_or(tokens, pos)?;
            if tokens.get(*pos) != Some(&")") {
                return Err("a `(` is not closed".into());
            }
            *pos += 1;
            return Ok(inner);
        }
        if !is_id(token) {
            return Err(format!("`{token}` is not a license identifier"));
        }
        let mut exception = None;
        if tokens.get(*pos) == Some(&"WITH") {
            let name = *tokens.get(*pos + 1).ok_or("WITH needs an exception")?;
            if !is_id(name) {
                return Err(format!("`{name}` is not an exception identifier"));
            }
            exception = Some(name.to_string());
            *pos += 2;
        }
        Ok(Expr::License { id: token.to_string(), exception })
    }

    /// Whether some choice among the ORs uses allowed licenses only. An
    /// exception must be allowed as a whole, as in `Apache-2.0 WITH
    /// LLVM-exception`.
    fn satisfied_by(&self, allowed: &[String]) -> bool {
        match self {
            Expr::License { id, exception: None } => allowed.iter().any(|a| a == id),
            Expr::License { id, exception: Some(exception) } => {
                allowed.iter().any(|a| *a == format!("{id} WITH {exception}"))
            }
            Expr::And(a, b) => a.satisfied_by(allowed) && b.satisfied_by(allowed),
            Expr::Or(a, b) => a.satisfied_by(allowed) || b.satisfied_by(allowed),
        }
    }

    fn collect_ids(&self, ids: &mut BTreeSet<String>) {
        match self {
            Expr::License { id, exception } => {
                ids.insert(id.clone());
                ids.extend(exception.clone());
            }
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.collect_ids(ids);
                b.collect_ids(ids);
            }
        }
    }
}

fn is_id(token: &str) -> bool {
    !matches!(token, "AND" | "OR" | "WITH" | "(" | ")")
        && token.chars().all(|c| c.is_ascii_alphanumeric() || ".-+:".contains(c))
}

/// SPDX identifiers allow letters, digits, `.` and `-` only.
fn spdx_id(name: &str) -> String {
    let clean: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect();
    format!("SPDXRef-Package-{clean}")
}

fn git_commit(ctx: &Ctx) -> String {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&ctx.root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// The SPDX document's own fields.
struct Document {
    name: String,
    commit: String,
    /// Seconds since 1970, from `SOURCE_DATE_EPOCH` or the commit time.
    created: i64,
}

impl Document {
    /// Renders the document in the SPDX 2.3 tag-value format.
    fn render(&self, packages: &[Package], ids: &BTreeSet<String>, root: &Path) -> Result<String> {
        let mut out = String::new();
        let o = &mut out;
        let _ = writeln!(o, "SPDXVersion: SPDX-2.3");
        let _ = writeln!(o, "DataLicense: CC0-1.0");
        let _ = writeln!(o, "SPDXID: SPDXRef-DOCUMENT");
        let _ = writeln!(o, "DocumentName: {}", self.name);
        let _ = writeln!(
            o,
            "DocumentNamespace: https://github.com/openlinuxsystems/orthros/spdx/{}-{}",
            self.name, self.commit
        );
        let _ = writeln!(o, "Creator: Tool: ob-{VERSION}");
        let _ = writeln!(o, "Created: {}", rfc3339(self.created));
        let _ = writeln!(o);
        let _ = writeln!(o, "PackageName: orthros");
        let _ = writeln!(o, "SPDXID: SPDXRef-Package-orthros");
        let _ = writeln!(o, "PackageVersion: {VERSION}");
        let _ = writeln!(
            o,
            "PackageDownloadLocation: git+https://github.com/openlinuxsystems/orthros@{}",
            self.commit
        );
        let _ = writeln!(o, "FilesAnalyzed: false");
        let _ = writeln!(o, "PackageLicenseConcluded: NOASSERTION");
        let _ = writeln!(o, "PackageLicenseDeclared: NOASSERTION");
        let _ = writeln!(o, "PackageCopyrightText: NOASSERTION");
        let _ = writeln!(
            o,
            "PackageComment: <text>The OrthrOS {VERSION} x86-64 boot image. Each \
             component below keeps its own license; the texts are in \
             /{DIR}/texts and the notices in /{DIR}/notices.</text>"
        );
        let _ = writeln!(o, "Relationship: SPDXRef-DOCUMENT DESCRIBES SPDXRef-Package-orthros");
        for package in packages {
            let _ = writeln!(o);
            let _ = writeln!(o, "PackageName: {}", package.name);
            let _ = writeln!(o, "SPDXID: {}", package.spdx_id());
            // SPDX has no NOASSERTION for versions; an unknown one is left out.
            if package.version != "NOASSERTION" {
                let _ = writeln!(o, "PackageVersion: {}", package.version);
            }
            let _ = writeln!(o, "PackageDownloadLocation: {}", package.download);
            let _ = writeln!(o, "FilesAnalyzed: false");
            if let Some(sum) = &package.sha256 {
                let _ = writeln!(o, "PackageChecksum: SHA256: {sum}");
            }
            let _ = writeln!(o, "PackageLicenseConcluded: {}", package.license);
            let _ = writeln!(o, "PackageLicenseDeclared: {}", package.license);
            let _ = writeln!(o, "PackageCopyrightText: NOASSERTION");
            let mut comment = package.comment.clone();
            if !package.notices.is_empty() {
                let _ = write!(comment, " Notices: /{DIR}/{}.", package.notice_dir());
            }
            let _ = writeln!(o, "PackageComment: <text>{comment}</text>");
            let _ =
                writeln!(o, "Relationship: SPDXRef-Package-orthros CONTAINS {}", package.spdx_id());
        }
        for id in ids.iter().filter(|id| id.starts_with("LicenseRef-")) {
            let path = text_source(root, id);
            let text =
                std::fs::read_to_string(&path).context(|| format!("reading {}", path.display()))?;
            let _ = writeln!(o);
            let _ = writeln!(o, "LicenseID: {id}");
            let _ = writeln!(o, "ExtractedText: <text>{}</text>", text.trim_end());
            let _ = writeln!(o, "LicenseName: {}", id.trim_start_matches("LicenseRef-"));
        }
        Ok(out)
    }
}

/// A one-line-per-component summary for people reading the image.
fn readme(packages: &[Package]) -> String {
    let mut out = format!(
        "OrthrOS {VERSION} third-party and component licenses\n\n\
         orthros.spdx lists every component of this image in the SPDX 2.3\n\
         tag-value format. texts/ holds the full text of every license named\n\
         there, and notices/ the copyright and license files that came with\n\
         each component's source.\n\n"
    );
    let width = packages.iter().map(|p| p.name.len() + p.version.len() + 1).max().unwrap_or(0);
    for package in packages {
        let what = format!("{} {}", package.name, package.version);
        let _ = writeln!(out, "{what:width$}  {}", package.license);
    }
    out
}

/// Formats seconds since 1970 as `YYYY-MM-DDThh:mm:ssZ`.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn allow(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_expressions() {
        let expr =
            Expr::parse("MIT AND Apache-2.0 WITH LLVM-exception AND (MIT OR Apache-2.0)").unwrap();
        let mut ids = BTreeSet::new();
        expr.collect_ids(&mut ids);
        assert_eq!(ids, BTreeSet::from(["Apache-2.0", "LLVM-exception", "MIT"].map(String::from)));
        assert!(Expr::parse("MIT/Apache-2.0").unwrap().satisfied_by(&allow(&["MIT"])));
        assert!(Expr::parse("MIT AND").is_err());
        assert!(Expr::parse("(MIT").is_err());
        assert!(Expr::parse("MIT Apache-2.0").is_err());
    }

    #[test]
    fn checks_allow_lists() {
        let ok = |text: &str, list: &[&str]| Expr::parse(text).unwrap().satisfied_by(&allow(list));
        assert!(ok("Apache-2.0 OR MIT", &["MIT"]));
        assert!(!ok("GPL-2.0-only", &["MIT", "Apache-2.0"]));
        assert!(ok("MIT OR GPL-2.0-only", &["MIT"]));
        assert!(!ok("MIT AND GPL-2.0-only", &["MIT"]));
        assert!(!ok("Apache-2.0 WITH LLVM-exception", &["Apache-2.0"]));
        assert!(ok("Apache-2.0 WITH LLVM-exception", &["Apache-2.0 WITH LLVM-exception"]));
    }

    #[test]
    fn parses_cargo_tree_lines() {
        assert_eq!(
            parse_tree_line("radix-klog v0.0.1 (/src/kernel/lib/klog)|Apache-2.0 OR MIT (*)"),
            Some(TreeEntry {
                name: "radix-klog".into(),
                version: "0.0.1".into(),
                source: Source::Path("/src/kernel/lib/klog".into()),
                license: "Apache-2.0 OR MIT".into(),
            })
        );
        let entry = parse_tree_line("bitflags v2.13.2|MIT OR Apache-2.0").unwrap();
        assert_eq!(entry.source, Source::Registry);
        let entry = parse_tree_line("x v1.0.0 (https://example.com/x#abc)|MIT").unwrap();
        assert!(matches!(entry.source, Source::Other(_)));
        assert_eq!(parse_tree_line("").map(|e| e.name), None);
    }

    /// The kernel area is permissive only (ADR 0021): copyleft licenses
    /// belong to separate programs and bundles, never to kernel code.
    #[test]
    fn kernel_policy_is_permissive() {
        let text = std::fs::read_to_string(root().join(KERNEL_POLICY)).unwrap();
        for id in parse_allow_list(&text).unwrap() {
            for copyleft in ["GPL", "LGPL", "AGPL", "MPL", "EPL", "CDDL", "EUPL", "OSL", "SSPL"] {
                assert!(!id.contains(copyleft), "kernel/deny.toml allows {id}");
            }
        }
    }

    /// Every license the kernel area allows, and every license a fixed
    /// area allows, has its text ready to copy into the image.
    #[test]
    fn allowed_licenses_have_texts() {
        let text = std::fs::read_to_string(root().join(KERNEL_POLICY)).unwrap();
        let mut ids = BTreeSet::new();
        for id in parse_allow_list(&text).unwrap() {
            Expr::parse(&id).unwrap().collect_ids(&mut ids);
        }
        for id in BOOTLOADER_ALLOW.iter().chain(FIRMWARE_ALLOW) {
            Expr::parse(id).unwrap().collect_ids(&mut ids);
        }
        for id in ids {
            let path = text_source(&root(), &id);
            assert!(path.is_file(), "{} is missing", path.display());
        }
    }

    /// No license text is kept twice, and the copies at the top of the
    /// repository, which GitHub and crate tools look for, match LICENSES/.
    #[test]
    fn license_texts_are_kept_once() {
        let root = root();
        for entry in std::fs::read_dir(root.join("LICENSES")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let id = name.strip_suffix(".txt").unwrap();
            let other = root.join("image/licenses").join(format!("LICENSE-{id}.txt"));
            assert!(!other.exists(), "{} duplicates LICENSES/{name}", other.display());
        }
        for (top, id) in [("LICENSE-APACHE", "Apache-2.0"), ("LICENSE-MIT", "MIT")] {
            let top = std::fs::read(root.join(top)).unwrap();
            assert!(top == std::fs::read(text_source(&root, id)).unwrap(), "{id} differs");
        }
    }

    #[test]
    fn finds_build_std_crates() {
        let args = ["--target".into(), "x.json".into(), "-Zbuild-std=core,alloc".into()];
        assert_eq!(build_std_crates(&args), ["core", "alloc"]);
        let names: Vec<&str> = SYSROOT_CRATES.iter().map(|(name, _)| *name).collect();
        for name in build_std_crates(&["-Zbuild-std=core,compiler_builtins,alloc".into()]) {
            assert!(names.contains(&name), "{name}");
        }
    }

    #[test]
    fn formats_times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_792_537_558), "2026-10-20T23:05:58Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn renders_spdx() {
        let packages = vec![Package {
            name: "compiler_builtins".into(),
            version: "0.1.160".into(),
            license: "MIT".into(),
            area: Area::Kernel,
            download: "NOASSERTION".into(),
            sha256: Some("ab".into()),
            notices: vec!["LICENSE.txt".into()],
            comment: "C.".into(),
        }];
        let document =
            Document { name: "orthros-test".into(), commit: "c0ffee".into(), created: 0 };
        let ids = BTreeSet::from(["MIT".to_string()]);
        let text = document.render(&packages, &ids, &root()).unwrap();
        assert!(text.starts_with("SPDXVersion: SPDX-2.3\n"));
        assert!(text.contains("SPDXID: SPDXRef-Package-compiler-builtins-0.1.160\n"));
        assert!(text.contains("PackageChecksum: SHA256: ab\n"));
        assert!(text.contains(
            "Relationship: SPDXRef-Package-orthros CONTAINS \
             SPDXRef-Package-compiler-builtins-0.1.160\n"
        ));
        assert!(
            text.contains("Notices: /system/share/licenses/notices/compiler_builtins-0.1.160.")
        );
        assert!(text.contains("Created: 1970-01-01T00:00:00Z\n"));
    }

    #[test]
    fn grub_versions() {
        assert_eq!(grub_version("grub-mkrescue (GRUB) 2.12-1ubuntu7.3\n"), Some("2.12-1ubuntu7.3"));
        assert_eq!(grub_version(""), None);
    }
}
