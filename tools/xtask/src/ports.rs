// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ports: software from elsewhere, built for Radix (ob(1), PORTS; ADR 0069).
//!
//! A port is a directory `ports/<name>/` with:
//!
//! - `recipe.toml`: the port's version, license and home page, the git
//!   commit or the release archive's SHA-256 its source is pinned to (or,
//!   for Rust's standard library, the pinned Rust toolchain's rust-src),
//!   any other sources unpacked into its tree and crates from crates.io,
//!   each pinned by its SHA-256, the ports it needs built first, and the
//!   files it adds to the boot archive;
//! - `patches/*.patch`, applied in name order with `patch -p1`;
//! - `build.sh`, which builds the port in a fresh copy of its patched
//!   source and installs it into the sysroot (`out/sysroot`), or, for a
//!   port of the toolchain, into `out/toolchain`.
//!
//! Sources are fetched once into `target/ports`, with git by commit, as
//! release archives by checksum (from their home, or from a mirror when
//! that cannot be reached) and from crates.io by checksum, and checked to
//! be what the recipe names, so a build uses nothing else. Of a large
//! repository, such as LLVM's, a recipe can name the directories it
//! needs, and only their files are fetched. A port is
//! built again only when something that went into its last build
//! changed: its directory, the toolchain (and Rust's, for a port of
//! rust-src), or a port it needs.
//!
//! The toolchain is the host's Clang and LLD, [`CLANG_MIN`] or later,
//! made to target Radix by the wrapper scripts and Meson cross files ob
//! writes into `out/toolchain`: C for Radix against the C library in the
//! sysroot, linked into static position-independent programs, or, for
//! bundles that ship libraries of their own, into programs and libraries
//! on the shared C library and its dynamic linker (ADR 0071); C++ the same
//! way, on LLVM's libc++ once the llvm-runtimes port has built it; and a
//! CMake toolchain file for ports that build with CMake (ADR 0090).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result};

/// The oldest Clang the toolchain takes: mlibc's C++23 needs Clang 19's
/// concepts, and CI and the build container use 20.
pub const CLANG_MIN: u32 = 20;

/// The prefix of the toolchain's programs: `x86_64-radix-cc` and so on.
pub const TRIPLE: &str = "x86_64-radix";

/// One port's `recipe.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The port's name: its directory's.
    pub name: String,
    pub version: String,
    /// What it is, in a few words.
    pub summary: String,
    /// SPDX license expression of what the port installs.
    pub license: String,
    pub homepage: String,
    /// Ports built before this one.
    #[serde(default)]
    pub depends: Vec<String>,
    /// Whether it installs into the toolchain rather than the sysroot:
    /// what it installs is used to build, and never reaches an image.
    #[serde(default)]
    pub toolchain: bool,
    /// Its source; a port of crates alone has none.
    #[serde(default)]
    pub source: Source,
    /// More sources, unpacked inside the port's source tree.
    #[serde(default)]
    pub extra: Vec<Extra>,
    /// Crates from crates.io, unpacked inside the port's source tree.
    #[serde(default, rename = "crate")]
    pub crates: Vec<Crate>,
    /// Files of the sysroot the boot archive holds, as paths beneath it;
    /// one that ends in `/` is a directory, with everything in it.
    #[serde(default)]
    pub image: Vec<String>,
    /// The files of the source that hold its copyright and license
    /// notice, for a source without a LICENSE or COPYING file.
    #[serde(default)]
    pub notices: Vec<String>,
    /// Code from other projects that the port keeps in its own
    /// directory, each a package of its own in the license manifest.
    #[serde(default)]
    pub bundled: Vec<Bundled>,
}

/// Where a source comes from: a git repository and a commit in it, a
/// release archive, or rust-src; none of them for a port of crates alone.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The repository, for a source from git.
    #[serde(default)]
    pub git: String,
    /// The full commit ID, which pins the tree.
    #[serde(default)]
    pub commit: String,
    /// The release or tag the commit is, for people.
    #[serde(default)]
    pub tag: Option<String>,
    /// The directories of the repository the port needs, for a large one
    /// of which it needs only these: only their files are fetched (with a
    /// partial clone, checked out sparsely) and copied into its tree.
    #[serde(default)]
    pub paths: Vec<String>,
    /// A release archive (`.tar.gz`, `.tar.xz` or `.tar.bz2`), for a
    /// project whose releases hold more than its repository does, such as
    /// GNU's, whose archives hold their generated configure scripts.
    #[serde(default)]
    pub url: String,
    /// The archive's SHA-256, which pins it.
    #[serde(default)]
    pub sha256: String,
    /// Other places that serve the same archive, tried in order when
    /// `url` cannot be reached: Ubuntu's source pool keeps upstream's
    /// release archives, unchanged, as `<name>_<version>.orig.tar.*`.
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Whether the source is the `library` directory of the pinned Rust
    /// toolchain's rust-src component, with the crates it vendors: the
    /// toolchain pins it, and it needs no fetching.
    #[serde(default, rename = "rust-src")]
    pub rust_src: bool,
}

impl Source {
    /// Where the source can be downloaded, as an SPDX download location.
    pub fn download(&self) -> String {
        if !self.url.is_empty() {
            return self.url.clone();
        }
        format!("git+{}@{}", self.git, self.commit)
    }

    /// Which release it is, for people: its tag, its commit or its
    /// archive's name.
    pub fn release(&self) -> &str {
        match &self.tag {
            Some(tag) => tag,
            None if !self.url.is_empty() => self.url.rsplit('/').next().unwrap_or(&self.url),
            None => &self.commit,
        }
    }
}

/// A source unpacked inside a port's tree, such as a Meson subproject.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extra {
    /// Where it goes, relative to the port's source.
    pub path: String,
    pub git: String,
    pub commit: String,
    /// SPDX license expression.
    pub license: String,
}

/// A crate from crates.io unpacked inside a port's tree: the crates a port
/// of Rust's standard library or of crates patches for Radix.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Crate {
    pub name: String,
    pub version: String,
    /// The SHA-256 of its `.crate` file, which Cargo.lock files record too.
    pub sha256: String,
    /// SPDX license expression, as its Cargo.toml gives it.
    pub license: String,
    /// Where it goes, relative to the port's source: `<name>-<version>`
    /// unless given.
    #[serde(default)]
    pub path: Option<String>,
}

impl Crate {
    /// Where it goes in the port's source.
    pub fn path(&self) -> String {
        self.path.clone().unwrap_or_else(|| format!("{}-{}", self.name, self.version))
    }
}

/// Code from another project that a port keeps in its own directory, as
/// ports/mlibc keeps musl's in `musl/`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundled {
    pub name: String,
    pub version: String,
    /// SPDX license expression.
    pub license: String,
    pub homepage: String,
    /// The files that hold its copyright and license notice, relative to
    /// the port's directory.
    pub notices: Vec<String>,
}

/// A port that was built: its recipe, its directory, and where its
/// sources are.
pub struct Port {
    pub recipe: Recipe,
    /// `ports/<name>`.
    pub dir: PathBuf,
    /// Its source as fetched, if it has one.
    pub source: Option<PathBuf>,
    /// The extra sources, in the recipe's order.
    pub extras: Vec<PathBuf>,
    /// The crates, unpacked, in the recipe's order.
    pub crates: Vec<PathBuf>,
}

/// Reads every recipe in `ports/`, in an order that builds each port
/// after those it needs.
pub fn recipes(ctx: &Ctx) -> Result<Vec<(Recipe, PathBuf)>> {
    let ports = ctx.root.join("ports");
    let mut found = BTreeMap::new();
    let entries = std::fs::read_dir(&ports).context(|| format!("reading {}", ports.display()))?;
    for entry in entries {
        let dir = entry.context(|| format!("reading {}", ports.display()))?.path();
        let file = dir.join("recipe.toml");
        if !file.is_file() {
            continue;
        }
        let text =
            std::fs::read_to_string(&file).context(|| format!("reading {}", file.display()))?;
        let recipe: Recipe = toml::from_str(&text)
            .map_err(|err| Error::new(format!("{}: {err}", ctx.display(&file))))?;
        check(ctx, &recipe, &dir)?;
        found.insert(recipe.name.clone(), (recipe, dir));
    }
    // Each port after those it needs: a depth-first walk.
    let mut order = Vec::new();
    let names: Vec<String> = found.keys().cloned().collect();
    let mut state: BTreeMap<String, bool> = BTreeMap::new();
    for name in &names {
        visit(name, &found, &mut state, &mut order)?;
    }
    Ok(order
        .into_iter()
        .map(|name| found.remove(&name).expect("every name visited was found"))
        .collect())
}

fn visit(
    name: &str,
    found: &BTreeMap<String, (Recipe, PathBuf)>,
    state: &mut BTreeMap<String, bool>,
    order: &mut Vec<String>,
) -> Result<()> {
    match state.get(name) {
        Some(true) => return Ok(()),
        Some(false) => return Err(Error::new(format!("ports: {name} needs itself"))),
        None => {}
    }
    let Some((recipe, _)) = found.get(name) else {
        return Err(Error::new(format!("ports: no port is named {name}")));
    };
    state.insert(name.to_string(), false);
    for dep in &recipe.depends {
        visit(dep, found, state, order)
            .map_err(|err| Error::new(format!("ports: {name} needs {dep}: {err}")))?;
    }
    state.insert(name.to_string(), true);
    order.push(name.to_string());
    Ok(())
}

/// Checks what a recipe says that serde does not.
fn check(ctx: &Ctx, recipe: &Recipe, dir: &Path) -> Result<()> {
    let what = ctx.display(&dir.join("recipe.toml"));
    if dir.file_name().and_then(|n| n.to_str()) != Some(recipe.name.as_str()) {
        return Err(Error::new(format!("{what}: the name must be the directory's")));
    }
    let source = &recipe.source;
    let (git, archive) = (!source.git.is_empty(), !source.url.is_empty());
    let kinds = usize::from(git) + usize::from(archive) + usize::from(source.rust_src);
    if kinds > 1 {
        return Err(Error::new(format!(
            "{what}: a source is one of git, url and rust-src, not more"
        )));
    }
    if source.rust_src && !recipe.toolchain {
        return Err(Error::new(format!("{what}: only a toolchain port builds rust-src")));
    }
    if kinds == 0 && recipe.crates.is_empty() {
        return Err(Error::new(format!(
            "{what}: a port needs a source: git and commit, url and sha256, rust-src or crates"
        )));
    }
    if !git && (!source.commit.is_empty() || source.tag.is_some()) {
        return Err(Error::new(format!("{what}: commit and tag go with git")));
    }
    if !archive && (!source.sha256.is_empty() || !source.mirrors.is_empty()) {
        return Err(Error::new(format!("{what}: sha256 and mirrors go with url")));
    }
    if archive && !is_hex(&source.sha256, 64) {
        return Err(Error::new(format!("{what}: `{}` is not a SHA-256", source.sha256)));
    }
    for url in archive.then_some(&source.url).into_iter().chain(&source.mirrors) {
        if !url.starts_with("https://") || url.ends_with('/') {
            return Err(Error::new(format!("{what}: `{url}` is not an https:// URL of a file")));
        }
    }
    let main = git.then_some(&source.commit);
    for commit in main.into_iter().chain(recipe.extra.iter().map(|e| &e.commit)) {
        if !is_hex(commit, 40) {
            return Err(Error::new(format!("{what}: `{commit}` is not a full commit ID")));
        }
    }
    for krate in &recipe.crates {
        let path = krate.path();
        if !is_hex(&krate.sha256, 64) {
            return Err(Error::new(format!(
                "{what}: crate {} {}: `{}` is not a SHA-256",
                krate.name, krate.version, krate.sha256
            )));
        }
        let plain = |s: &str| {
            !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
        };
        if !plain(&krate.name) || !plain(&krate.version) || !inside(&path) {
            return Err(Error::new(format!(
                "{what}: crate `{}` `{}` at `{path}` is not a crate's name, version and \
                 a path inside the source",
                krate.name, krate.version
            )));
        }
    }
    if let Some(path) = outside_path(recipe) {
        return Err(Error::new(format!(
            "{what}: `{path}` is not a path inside the tree it is taken from"
        )));
    }
    if !dir.join("build.sh").is_file() {
        return Err(Error::new(format!("{what}: the port has no build.sh")));
    }
    if recipe.toolchain && !recipe.image.is_empty() {
        return Err(Error::new(format!("{what}: a toolchain port puts nothing in the image")));
    }
    Ok(())
}

/// The first path `recipe` names that leaves the tree it is taken from:
/// an extra source's place in the source, the source's paths, the
/// sysroot's files for the image (a directory ends in `/`), and the
/// notices in the source or in the port's directory.
fn outside_path(recipe: &Recipe) -> Option<&str> {
    let image = recipe.image.iter().map(|p| p.strip_suffix('/').unwrap_or(p));
    let bundled = recipe.bundled.iter().flat_map(|b| b.notices.iter().map(String::as_str));
    recipe
        .extra
        .iter()
        .map(|e| e.path.as_str())
        .chain(recipe.source.paths.iter().map(String::as_str))
        .chain(image)
        .chain(recipe.notices.iter().map(String::as_str))
        .chain(bundled)
        .find(|path| !inside(path))
}

/// Whether `path` is a relative path that stays inside the directory it
/// is relative to.
fn inside(path: &str) -> bool {
    !path.starts_with('/') && path.split('/').all(|part| !part.is_empty() && part != "..")
}

/// Whether `text` is `len` hexadecimal digits.
fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The host programs the toolchain is made of.
pub struct Toolchain {
    /// `out/toolchain`.
    pub dir: PathBuf,
    /// The first line of `clang --version`.
    pub version: String,
    clang: PathBuf,
    resource: PathBuf,
    builtins: PathBuf,
    tools: BTreeMap<&'static str, PathBuf>,
}

/// The LLVM tools the toolchain wraps, by the name the wrapper gets.
pub const TOOLS: &[(&str, &str)] = &[
    ("ar", "llvm-ar"),
    ("ranlib", "llvm-ranlib"),
    ("nm", "llvm-nm"),
    ("strip", "llvm-strip"),
    ("objcopy", "llvm-objcopy"),
    ("readelf", "llvm-readelf"),
    ("ld", "ld.lld"),
];

/// The first of `name-N` for N from the newest down to [`CLANG_MIN`], or
/// `name`, that is on `PATH`.
pub fn find_llvm(name: &str) -> Option<PathBuf> {
    (CLANG_MIN..CLANG_MIN + 10)
        .rev()
        .find_map(|major| process::find_program(format!("{name}-{major}")))
        .or_else(|| process::find_program(name))
}

/// Clang's major version from the first line of `clang --version`.
fn clang_major(line: &str) -> Option<u32> {
    let after = line.split("clang version ").nth(1)?;
    after.split('.').next()?.parse().ok()
}

/// Clang, [`CLANG_MIN`] or later, and the first line of its `--version`.
pub fn find_clang() -> Option<(PathBuf, String)> {
    let clang = find_llvm("clang")?;
    let out = Command::new(&clang).arg("--version").output().ok()?;
    let version = String::from_utf8_lossy(&out.stdout).lines().next()?.to_string();
    (clang_major(&version)? >= CLANG_MIN).then_some((clang, version))
}

/// `clang`'s resource directory, and compiler-rt's x86-64 builtins in it.
pub fn find_builtins(clang: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = Command::new(clang).arg("-print-resource-dir").output().ok()?;
    let resource = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // Distributions put them in one of these.
    let builtins = [
        "lib/linux/libclang_rt.builtins-x86_64.a",
        "lib/x86_64-pc-linux-gnu/libclang_rt.builtins.a",
        "lib/x86_64-unknown-linux-gnu/libclang_rt.builtins.a",
        "lib/x86_64-redhat-linux-gnu/libclang_rt.builtins.a",
    ]
    .iter()
    .map(|path| resource.join(path))
    .find(|path| path.is_file())?;
    Some((resource, builtins))
}

/// Finds Clang, LLD and the LLVM tools, and compiler-rt's builtins.
pub fn find_toolchain(ctx: &Ctx) -> Result<Toolchain> {
    let missing = || {
        Error::new(format!(
            "ports need Clang {CLANG_MIN} or later, LLD, compiler-rt and LLVM's tools"
        ))
        .hint("run `./ob doctor` to see what to install")
    };
    let (clang, version) = find_clang().ok_or_else(missing)?;
    let (resource, builtins) = find_builtins(&clang).ok_or_else(missing)?;
    let mut tools = BTreeMap::new();
    for (name, program) in TOOLS {
        let path = find_llvm(program).ok_or_else(missing)?;
        tools.insert(*name, path);
    }
    Ok(Toolchain { dir: ctx.out.join("toolchain"), version, clang, resource, builtins, tools })
}

impl Toolchain {
    pub fn bin(&self) -> PathBuf {
        self.dir.join("bin")
    }

    /// The C++ headers the C library is built with: libstdc++'s
    /// freestanding ones, from the freestnd-cxx-hdrs port.
    fn cxx_headers(&self) -> PathBuf {
        self.dir.join("include/c++")
    }

    /// The Meson cross file for building the C library itself.
    pub fn libc_cross_file(&self) -> PathBuf {
        self.dir.join("meson/radix-libc.ini")
    }

    /// The Meson cross file for building programs and libraries.
    pub fn cross_file(&self) -> PathBuf {
        self.dir.join("meson/radix.ini")
    }

    /// LLVM's version, `20.1.2` in `Ubuntu clang version 20.1.2 (...)`.
    pub fn llvm_version(&self) -> String {
        self.version
            .split("clang version ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("NOASSERTION")
            .to_string()
    }

    /// The C compiler wrapper.
    pub fn cc(&self) -> PathBuf {
        self.bin().join(format!("{TRIPLE}-cc"))
    }

    /// The C++ compiler wrapper.
    pub fn cxx(&self) -> PathBuf {
        self.bin().join(format!("{TRIPLE}-c++"))
    }

    /// Clang's C++ driver, beside Clang: clang++-20 for clang-20.
    fn clangxx(&self) -> PathBuf {
        let name = self.clang.file_name().and_then(|n| n.to_str()).unwrap_or("clang");
        self.clang.with_file_name(name.replacen("clang", "clang++", 1))
    }

    /// CMake's files for Radix: the toolchain file, and the platform
    /// files it finds Radix's description in.
    pub fn cmake(&self) -> PathBuf {
        self.dir.join("cmake")
    }

    /// The CMake toolchain file, for `-DCMAKE_TOOLCHAIN_FILE=`.
    pub fn cmake_toolchain_file(&self) -> PathBuf {
        self.cmake().join("radix.cmake")
    }

    /// Cargo for Radix: the wrapper that builds programs on Rust's
    /// standard library.
    pub fn cargo(&self) -> PathBuf {
        self.bin().join(format!("{TRIPLE}-cargo"))
    }

    /// The sysroot of Rust's standard library for Radix, which the
    /// rust-std port builds, with the crates the ports patch for Radix.
    pub fn rust(&self) -> PathBuf {
        self.dir.join("rust")
    }

    /// The `[patch]` configurations of the crates the ports patch for
    /// Radix, one per crate, named `<name>-<version>.toml`.
    pub fn crate_patches(&self) -> PathBuf {
        self.rust().join("patches")
    }

    /// The autoconf site file for Radix, and what else configure scripts
    /// need beyond the wrappers.
    pub fn autotools(&self) -> PathBuf {
        self.dir.join("autotools")
    }

    /// Writes the wrapper scripts and cross files into `out/toolchain`.
    pub fn write(&self, ctx: &Ctx) -> Result<()> {
        process::create_dir(&self.bin())?;
        process::create_dir(&self.dir.join("meson"))?;
        process::create_dir(&self.autotools())?;
        process::create_dir(&self.cmake().join("Platform"))?;
        let sysroot = ctx.out.join("sysroot");
        write_script(&self.cc(), &self.compiler_script(&sysroot, false))?;
        write_script(&self.cxx(), &self.compiler_script(&sysroot, true))?;
        let script = |name: &str, path: &Path| {
            format!(
                "#!/bin/sh\n# Written by ob (ob(1), PORTS): the Radix toolchain's {name}.\n\
                 exec '{}' \"$@\"\n",
                path.display()
            )
        };
        for (name, path) in &self.tools {
            write_script(&self.bin().join(format!("{TRIPLE}-{name}")), &script(name, path))?;
        }
        // The C library is built for no OS, for which Clang hands linking
        // to the host's GCC, and GCC takes `-fuse-ld=lld` to mean the first
        // `ld.lld` on PATH: this one, the toolchain's own LLD, rather than
        // whichever the host has, if any (Ubuntu's lld-20 installs only
        // `ld.lld-20`).
        write_script(&self.bin().join("ld.lld"), &script("ld", &self.tools["ld"]))?;
        write_script(&self.cargo(), &self.cargo_script(ctx)?)?;
        // Ports with GNU configure scripts (autoconf's) run them with this.
        write_script(
            &self.bin().join(format!("{TRIPLE}-configure")),
            include_str!("../autotools/configure.sh"),
        )?;
        write_file(
            &self.autotools().join("config.site"),
            include_str!("../autotools/config.site"),
        )?;
        write_file(&self.libc_cross_file(), &self.libc_cross(&sysroot))?;
        write_file(&self.cross_file(), &self.programs_cross())?;
        self.write_cmake(&sysroot)?;
        Ok(())
    }

    /// The C compiler: Clang for Linux's x86-64 ABI, which is Radix's
    /// (radix-posix(7)), with Linux's own macros taken away and Radix's
    /// given, the sysroot's headers only, and, when it links, the C
    /// library's start files, the C library and compiler-rt's builtins,
    /// without the functions and data it never uses (each is in a section
    /// of its own, as the C library's are, for the linker to drop). It
    /// links a static position-independent program, as system programs
    /// are; with `-dynamic`, which it takes away, a program that the
    /// dynamic linker starts (`/system/lib/ld.so`), and with `-shared` a
    /// shared library, both on the shared C library, for bundles that ship
    /// libraries of their own (ADR 0071). Linux's target, rather than
    /// none, makes Clang link with LLD itself (for no OS it hands linking
    /// to the host's GCC).
    ///
    /// The C++ compiler is the same with Clang's C++ driver, libc++'s
    /// headers before the C library's (as libc++ needs: its own
    /// `<stdlib.h>` and the like wrap the C library's), and, when it
    /// links, libc++, libc++abi and libunwind before the C library (ADR
    /// 0090). `-nostdinc++` leaves out the headers and `-nostdlib++` or
    /// `-nodefaultlibs` the libraries, as libc++'s own build asks.
    fn compiler_script(&self, sysroot: &Path, cxx: bool) -> String {
        let lib = sysroot.join("system/lib");
        let (what, clang) =
            if cxx { ("C++ compiler", self.clangxx()) } else { ("C compiler", self.clang.clone()) };
        // What only C++ adds: the arguments that leave out libc++, its
        // headers, and its libraries.
        let (cases, headers, libraries) = if cxx {
            (
                "\t-nostdinc++)\n\t\tcxxinc= ;;\n\
                 \t-nostdlib++ | -nodefaultlibs)\n\t\tcxxlib= ;;\n",
                format!(
                    "[ -n \"$cxxinc\" ] && set -- -isystem '{}/system/include/c++/v1' \"$@\"\n\
                     set -- -nostdinc++ \"$@\"\n",
                    sysroot.display()
                ),
                "-lc++ -lc++abi -lunwind",
            )
        } else {
            ("", String::new(), "")
        };
        let mut s = String::new();
        let _ = write!(
            s,
            r#"#!/bin/sh
# Written by ob (ob(1), PORTS): the Radix toolchain's {what}.
# {version}
link=yes
mode=static
cxxinc=yes
cxxlib=yes
for arg do
	shift
	case $arg in
	-c | -S | -E | -M | -MM | -fsyntax-only | -print-* | -dump* | --version | -v | -'###')
		link= ;;
	-shared)
		mode=shared ;;
	-dynamic)
		mode=dynamic
		continue ;;
	# The target is this script's to choose: the cc crate names Rust's
	# (x86_64-unknown-radix-mlibc), which Clang does not know.
	--target=*)
		continue ;;
{cases}	esac
	set -- "$@" "$arg"
done
set -- -isystem '{sysroot}/system/include' -isystem '{resource}/include' \
	-fPIE -ffunction-sections -fdata-sections "$@"
{headers}set -- --target=x86_64-unknown-linux-musl \
	-U__linux__ -U__linux -Ulinux -U__gnu_linux__ \
	-D__radix__ -D__dukovo__ \
	--sysroot='{sysroot}' -nostdinc "$@"
if [ -n "$link" ]; then
	# The C library goes in as the program does, whichever way the
	# arguments left the linker (rustc's end with -Bdynamic).
	case $mode in
	static) start='{lib}/Scrt1.o' libc=-Bstatic; set -- -static-pie "$@" ;;
	dynamic) start='{lib}/Scrt1.o' libc=-Bdynamic
		set -- -pie -Wl,--dynamic-linker=/system/lib/ld.so -Wl,--as-needed "$@" ;;
	shared) start= libc=-Bdynamic; set -- -Wl,--as-needed "$@" ;;
	esac
	[ -n "$cxxlib" ] && cxx='{libraries}' || cxx=
	set -- -fuse-ld=lld -nostdlib -Wl,-z,max-page-size=4096 -Wl,--gc-sections \
		${{start:+"$start"}} '{lib}/crti.o' "$@" \
		-L'{lib}' -Wl,$libc $cxx -lc '{builtins}' '{lib}/crtn.o'
fi
exec '{clang}' "$@"
"#,
            version = self.version,
            sysroot = sysroot.display(),
            resource = self.resource.display(),
            lib = lib.display(),
            builtins = self.builtins.display(),
            clang = clang.display(),
        );
        s
    }

    /// CMake's toolchain file for Radix, and the platform files that
    /// describe it: a Unix whose programs and libraries are built as
    /// Linux's are (Radix's ABI is Linux's, radix-posix(7)), with its
    /// files under `/system` in the sysroot, where nothing of the host's
    /// is looked for.
    fn write_cmake(&self, sysroot: &Path) -> Result<()> {
        let tool = |name: &str| self.bin().join(format!("{TRIPLE}-{name}"));
        let mut text = format!(
            "# Written by ob (ob(1), PORTS): CMake's toolchain file for Radix.\n\
             list(APPEND CMAKE_MODULE_PATH \"{cmake}\")\n\
             set(CMAKE_SYSTEM_NAME Radix)\n\
             set(CMAKE_SYSTEM_PROCESSOR x86_64)\n\
             set(CMAKE_C_COMPILER \"{cc}\")\n\
             set(CMAKE_CXX_COMPILER \"{cxx}\")\n",
            cmake = self.cmake().display(),
            cc = self.cc().display(),
            cxx = self.cxx().display(),
        );
        for name in ["ar", "ranlib", "nm", "strip", "objcopy", "readelf"] {
            let _ =
                writeln!(text, "set(CMAKE_{} \"{}\")", name.to_uppercase(), tool(name).display());
        }
        let _ = write!(
            text,
            "set(CMAKE_SYSROOT \"{sysroot}\")\n\
             set(CMAKE_FIND_ROOT_PATH \"{sysroot}\")\n\
             set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)\n\
             set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)\n\
             set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)\n\
             set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)\n",
            sysroot = sysroot.display(),
        );
        write_file(&self.cmake_toolchain_file(), &text)?;
        // Read before the compilers are checked: what Radix is, and whose
        // compiler and linker settings it takes (CMake 3.28 and later).
        write_file(
            &self.cmake().join("Platform/Radix-Initialize.cmake"),
            "# Written by ob (ob(1), PORTS): Radix, for CMake.\n\
             set(UNIX 1)\n\
             set(CMAKE_EFFECTIVE_SYSTEM_NAME Linux)\n",
        )?;
        write_file(
            &self.cmake().join("Platform/Radix.cmake"),
            "# Written by ob (ob(1), PORTS): Radix, for CMake: Linux's\n\
             # settings, with /system first among the places to look.\n\
             include(Platform/Linux)\n\
             list(PREPEND CMAKE_SYSTEM_PREFIX_PATH /system)\n",
        )
    }

    /// Cargo for programs on Rust's standard library (ADR 0070), as
    /// `cargo` is used: `x86_64-radix-cargo build --release`. It builds for
    /// the x86_64-unknown-radix target against the rust-std port's
    /// sysroot, with the crates the ports patch for Radix in place of
    /// crates.io's (those the workspace's Cargo.lock names, so Cargo has no
    /// unused patches to warn of; all of them before there is one), and
    /// links with the toolchain's C compiler, which the cc crate builds C
    /// with too. The pinned Rust toolchain builds, whatever a port's tree
    /// pins.
    fn cargo_script(&self, ctx: &Ctx) -> Result<String> {
        let pin = ctx.root.join("rust-toolchain.toml");
        let text =
            std::fs::read_to_string(&pin).context(|| format!("reading {}", pin.display()))?;
        let channel = text
            .lines()
            .find_map(|line| line.strip_prefix("channel = "))
            .map(|value| value.trim().trim_matches('"').to_string())
            .ok_or_else(|| Error::new(format!("{} names no channel", ctx.display(&pin))))?;
        let tool = |name: &str| self.bin().join(format!("{TRIPLE}-{name}"));
        Ok(format!(
            r#"#!/bin/sh
# Written by ob (ob(1), PORTS): Cargo for Radix's programs on Rust's
# standard library.
if [ $# -eq 0 ]; then
	echo "usage: {TRIPLE}-cargo command [argument...]" >&2
	exit 2
fi
command=$1
shift
# The workspace's Cargo.lock: beside --manifest-path, or here or above.
dir=.
take=
for arg do
	[ -n "$take" ] && dir=$(dirname "$arg")
	take=
	case $arg in
	--manifest-path) take=yes ;;
	--manifest-path=*) dir=$(dirname "${{arg#*=}}") ;;
	esac
done
dir=$(cd "$dir" && pwd) || exit
lock=
while [ -z "$lock" ]; do
	if [ -f "$dir/Cargo.lock" ]; then
		lock=$dir/Cargo.lock
	elif [ "$dir" = / ]; then
		break
	fi
	dir=$(dirname "$dir")
done
for patch in '{patches}'/*.toml; do
	[ -f "$patch" ] || continue
	name=${{patch##*/}}
	name=${{name%-*}}
	if [ -z "$lock" ] || grep -qx "name = \"$name\"" "$lock"; then
		set -- --config "$patch" "$@"
	fi
done
case $command in
build | b | check | c | clippy | doc | d | fetch | run | r | rustc | test | t | tree)
	set -- --target '{spec}' -Zjson-target-spec "$@" ;;
esac
unset RUSTFLAGS CARGO_BUILD_RUSTFLAGS CARGO_BUILD_TARGET
export RUSTUP_TOOLCHAIN='{channel}'
export CARGO_ENCODED_RUSTFLAGS='--sysroot={rust}'
export CARGO_TARGET_X86_64_UNKNOWN_RADIX_LINKER='{cc}'
export CC_x86_64_unknown_radix='{cc}'
export AR_x86_64_unknown_radix='{ar}'
export CFLAGS_x86_64_unknown_radix=-O2
export CARGO_FUTURE_INCOMPAT_REPORT_FREQUENCY=never
exec cargo "$command" "$@"
"#,
            patches = self.crate_patches().display(),
            spec = ctx.root.join(crate::userspace::STD_TARGET_SPEC).display(),
            rust = self.rust().display(),
            cc = self.cc().display(),
            ar = tool("ar").display(),
        ))
    }

    /// The cross file the C library is built with: Clang for no OS, so
    /// that nothing of Linux's (ifuncs, say) leaks in; `unknown-attributes`
    /// as an error makes mlibc's ifunc check fail, as it must there. The
    /// sysroot's headers come after all others, for the kernel's
    /// (`radix/`) only: the C library has its own.
    fn libc_cross(&self, sysroot: &Path) -> String {
        let clangxx = self.clangxx();
        format!(
            "# Written by ob (ob(1), PORTS): Meson's cross file for the C library.\n\
             [binaries]\n\
             c = ['{clang}', '--target=x86_64-unknown-none-elf', '-nostdinc', '-isystem', '{res}/include']\n\
             cpp = ['{clangxx}', '--target=x86_64-unknown-none-elf', '-nostdinc', '-nostdinc++', \
             '-isystem', '{cxx}', '-isystem', '{res}/include', '-D_GNU_SOURCE']\n\
             ar = '{ar}'\n\
             strip = '{strip}'\n\
             c_ld = 'lld'\n\
             cpp_ld = 'lld'\n\
             \n\
             [built-in options]\n\
             c_args = ['-fPIC', '-ffunction-sections', '-fdata-sections', '-D__radix__', \
             '-D__dukovo__', '-D__unix__', '-idirafter', '{radix}']\n\
             cpp_args = ['-fPIC', '-ffunction-sections', '-fdata-sections', \
             '-Werror=unknown-attributes', '-D__radix__', '-D__dukovo__', '-D__unix__', \
             '-idirafter', '{radix}']\n\
             c_link_args = ['-nostdlib', '{builtins}']\n\
             cpp_link_args = ['-nostdlib', '{builtins}']\n\
             \n{host}",
            clang = self.clang.display(),
            clangxx = clangxx.display(),
            res = self.resource.display(),
            cxx = self.cxx_headers().display(),
            ar = self.tools["ar"].display(),
            strip = self.tools["strip"].display(),
            radix = sysroot.join("system/include").display(),
            builtins = self.builtins.display(),
            host = HOST_MACHINE,
        )
    }

    /// The cross file programs and libraries are built with.
    fn programs_cross(&self) -> String {
        let tool = |name: &str| self.bin().join(format!("{TRIPLE}-{name}"));
        format!(
            "# Written by ob (ob(1), PORTS): Meson's cross file for Radix.\n\
             [binaries]\n\
             c = '{cc}'\n\
             cpp = '{cxx}'\n\
             ar = '{ar}'\n\
             strip = '{strip}'\n\
             nm = '{nm}'\n\
             \n{host}",
            cc = self.cc().display(),
            cxx = self.cxx().display(),
            ar = tool("ar").display(),
            strip = tool("strip").display(),
            nm = tool("nm").display(),
            host = HOST_MACHINE,
        )
    }
}

/// Meson's description of the machine programs run on.
const HOST_MACHINE: &str = "[host_machine]\n\
system = 'radix'\n\
cpu_family = 'x86_64'\n\
cpu = 'x86_64'\n\
endian = 'little'\n";

fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).context(|| format!("writing {}", path.display()))
}

fn write_script(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    write_file(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .context(|| format!("making {} executable", path.display()))
}

/// The ports, built, and the toolchain they were built with.
pub struct Ports {
    pub toolchain: Toolchain,
    /// Every port, each after those it needs.
    pub ports: Vec<Port>,
}

/// Builds every port that changed, in order, and returns them all.
pub fn build_all(ctx: &Ctx, rebuild: &[String]) -> Result<Ports> {
    let toolchain = find_toolchain(ctx)?;
    toolchain.write(ctx)?;
    let mut stamps: BTreeMap<String, u64> = BTreeMap::new();
    let mut ports = Vec::new();
    for (recipe, dir) in recipes(ctx)? {
        let source = &recipe.source;
        let source = if source.rust_src {
            Some(rust_src(ctx)?.0)
        } else if !source.url.is_empty() {
            Some(fetch_archive(ctx, &recipe.name, source)?)
        } else if !source.git.is_empty() {
            Some(fetch(ctx, &recipe.name, &source.git, &source.commit, &source.paths)?)
        } else {
            None
        };
        let mut extras = Vec::new();
        for extra in &recipe.extra {
            let name = extra.path.rsplit('/').next().unwrap_or(&extra.path);
            extras.push(fetch(ctx, name, &extra.git, &extra.commit, &[])?);
        }
        let mut crates = Vec::new();
        for krate in &recipe.crates {
            crates.push(fetch_crate(ctx, krate)?);
        }
        let port = Port { recipe, dir, source, extras, crates };
        let recipe = &port.recipe;
        let stamp = inputs_hash(ctx, &port.dir, &toolchain, recipe, &stamps)?;
        let stamp_file = ctx.out.join("ports").join(format!("{}.stamp", recipe.name));
        let built = std::fs::read_to_string(&stamp_file).ok() == Some(format!("{stamp:016x}\n"));
        if built && !rebuild.contains(&recipe.name) {
            step(&format!("Port {} {} is up to date", recipe.name, recipe.version));
        } else {
            build(ctx, &toolchain, &port)?;
            // A port of Rust's standard library must agree with the C
            // library it calls.
            if recipe.source.rust_src {
                crate::libc_check::check(ctx, &toolchain)?;
            }
            process::create_dir(&ctx.out.join("ports"))?;
            write_file(&stamp_file, &format!("{stamp:016x}\n"))?;
        }
        stamps.insert(recipe.name.clone(), stamp);
        ports.push(port);
    }
    for name in rebuild {
        if !stamps.contains_key(name) {
            return Err(Error::new(format!("no port is named {name}"))
                .hint("the ports are the directories of ports/"));
        }
    }
    Ok(Ports { toolchain, ports })
}

/// What went into a port's build: its directory's files, the toolchain
/// (its version, and the wrappers, cross files and site file ob writes),
/// the kernel's system call headers, and the builds of the ports it needs.
fn inputs_hash(
    ctx: &Ctx,
    dir: &Path,
    toolchain: &Toolchain,
    recipe: &Recipe,
    stamps: &BTreeMap<String, u64>,
) -> Result<u64> {
    let mut hasher = DefaultHasher::new();
    let headers = ctx.out.join("sysroot/system/include/radix");
    let (wrappers, cross) = (toolchain.bin(), toolchain.dir.join("meson"));
    let (autotools, cmake) = (toolchain.autotools(), toolchain.cmake());
    for dir in [dir, &wrappers, &cross, &autotools, &cmake, &headers] {
        let mut files = Vec::new();
        list_files(dir, &mut files)?;
        files.sort();
        for file in &files {
            file.strip_prefix(dir).unwrap_or(file).hash(&mut hasher);
            std::fs::read(file)
                .context(|| format!("reading {}", file.display()))?
                .hash(&mut hasher);
        }
    }
    toolchain.version.hash(&mut hasher);
    toolchain.builtins.hash(&mut hasher);
    if recipe.source.rust_src {
        rust_src(ctx)?.1.hash(&mut hasher);
    }
    for dep in &recipe.depends {
        stamps.get(dep).hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn list_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).context(|| format!("reading {}", dir.display()))? {
        let entry = entry.context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        // A symbolic link is listed itself, even one to a directory.
        let kind = entry.file_type().context(|| format!("reading {}", path.display()))?;
        if kind.is_dir() {
            list_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// The pinned Rust toolchain's rust-src library, and its `rustc -vV`,
/// which says which it is.
fn rust_src(ctx: &Ctx) -> Result<(PathBuf, String)> {
    let mut cmd = Command::new("rustc");
    cmd.current_dir(&ctx.root).arg("-vV");
    let version = String::from_utf8_lossy(&process::output(ctx, &mut cmd)?.stdout).into_owned();
    let mut cmd = Command::new("rustc");
    cmd.current_dir(&ctx.root).args(["--print", "sysroot"]);
    let sysroot = String::from_utf8_lossy(&process::output(ctx, &mut cmd)?.stdout).into_owned();
    let library = PathBuf::from(sysroot.trim()).join("lib/rustlib/src/rust/library");
    if !library.join("vendor").is_dir() {
        return Err(Error::new(format!("rust-src is not in {}", library.display()))
            .hint("run `./ob setup`, which installs the toolchain's rust-src component"));
    }
    Ok((library, version))
}

/// Fetches commit `commit` of `git` into `target/ports/sources`, once,
/// and checks that it is that commit; with `paths`, only the files of
/// those directories. Returns where it is.
pub fn fetch(ctx: &Ctx, name: &str, git: &str, commit: &str, paths: &[String]) -> Result<PathBuf> {
    let dir = ctx.target.join("ports/sources").join(format!("{name}-{}", &commit[..12]));
    let head = |dir: &Path| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output();
        out.ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let git_in = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&dir).args(args);
        cmd
    };
    // In cone mode, which takes directories, each with what is beneath it.
    let sparse = || {
        let mut cmd = git_in(&["sparse-checkout", "set", "--cone", "--"]);
        cmd.args(paths);
        process::run(ctx, &mut cmd)
    };
    if dir.join(".git").is_dir() && head(&dir).as_deref() == Some(commit) {
        // The recipe may name other directories than it did.
        if !paths.is_empty() {
            sparse()?;
        }
        return Ok(dir);
    }
    step(&format!("Fetching {name} {} from {git}", &commit[..12]));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context(|| format!("removing {}", dir.display()))?;
    }
    process::create_dir(&dir)?;
    process::run(ctx, &mut git_in(&["init", "--quiet"]))?;
    if paths.is_empty() {
        process::run(ctx, &mut git_in(&["fetch", "--quiet", "--depth", "1", git, commit]))?;
    } else {
        // The commit and its trees only; git fetches the files of `paths`
        // as it checks them out, from the remote it records for that.
        process::run(ctx, &mut git_in(&["remote", "add", "origin", git]))?;
        let fetch = ["fetch", "--quiet", "--depth", "1", "--filter=blob:none", "origin", commit];
        process::run(ctx, &mut git_in(&fetch))?;
        sparse()?;
    }
    process::run(ctx, &mut git_in(&["checkout", "--quiet", "--detach", "FETCH_HEAD"]))?;
    match head(&dir) {
        Some(found) if found == commit => Ok(dir),
        found => Err(Error::new(format!(
            "{git} gave {} for commit {commit}",
            found.unwrap_or_else(|| "nothing".into())
        ))),
    }
}

/// Fetches a release archive into `target/ports/sources`, once, from its
/// URL or, failing that, from each mirror in turn, checks its SHA-256,
/// and unpacks it there. Returns where it is unpacked: the directory the
/// archive holds, as a release archive holds one, or else the whole.
fn fetch_archive(ctx: &Ctx, name: &str, source: &Source) -> Result<PathBuf> {
    let sources = ctx.target.join("ports/sources");
    let base = format!("{name}-{}", &source.sha256[..12]);
    // Unpacked beside its final place, then moved there: a directory
    // there is a whole archive.
    let dir = sources.join(&base);
    if dir.is_dir() {
        return Ok(dir);
    }
    process::create_dir(&sources)?;
    let file = sources.join(format!("{base}.download"));
    let mut failed = Vec::new();
    for url in std::iter::once(&source.url).chain(&source.mirrors) {
        step(&format!("Fetching {name} from {url}"));
        let mut cmd = Command::new("curl");
        cmd.args(["--proto", "=https", "--tlsv1.2", "-fsSL", "--retry", "3", "-o"])
            .arg(&file)
            .arg(url);
        if let Err(err) = process::run(ctx, &mut cmd) {
            failed.push(format!("{url}: {err}"));
            continue;
        }
        let sum = process::sha256(ctx, &file)?;
        if sum == source.sha256 {
            failed.clear();
            break;
        }
        failed.push(format!("{url} has SHA-256 {sum}, not the {} the recipe pins", source.sha256));
    }
    if !failed.is_empty() {
        let _ = std::fs::remove_file(&file);
        return Err(Error::new(format!(
            "no copy of {name}'s source would do: {}",
            failed.join("; ")
        ))
        .hint("add a mirror that serves the same archive to its recipe.toml"));
    }
    let partial = sources.join(format!("{base}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial).context(|| format!("removing {}", partial.display()))?;
    }
    process::create_dir(&partial)?;
    // tar finds out how the archive is compressed.
    let mut cmd = Command::new("tar");
    cmd.arg("-xf").arg(&file).arg("-C").arg(&partial).arg("--no-same-owner");
    process::run(ctx, &mut cmd)?;
    let entries: Vec<PathBuf> = std::fs::read_dir(&partial)
        .context(|| format!("reading {}", partial.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    let top = match entries.as_slice() {
        [one] if one.is_dir() => one.clone(),
        _ => partial.clone(),
    };
    std::fs::rename(&top, &dir).context(|| format!("moving it to {}", dir.display()))?;
    if partial.exists() {
        std::fs::remove_dir_all(&partial).context(|| format!("removing {}", partial.display()))?;
    }
    let _ = std::fs::remove_file(&file);
    Ok(dir)
}

/// Fetches a crate from crates.io into `target/ports/sources/crates`,
/// once, checks its SHA-256, and unpacks it there. Returns where it is
/// unpacked: with a `.cargo-checksum.json` that gives its checksum, as
/// Cargo wants of a vendored crate.
fn fetch_crate(ctx: &Ctx, krate: &Crate) -> Result<PathBuf> {
    let crates = ctx.target.join("ports/sources/crates");
    let name = format!("{}-{}", krate.name, krate.version);
    let dir = crates.join(&name);
    let checksum = format!("{{\"files\":{{}},\"package\":\"{}\"}}", krate.sha256);
    let marker = dir.join(".cargo-checksum.json");
    if std::fs::read_to_string(&marker).ok().as_deref() == Some(checksum.as_str()) {
        return Ok(dir);
    }
    let url = format!("https://static.crates.io/crates/{}/{name}.crate", krate.name);
    step(&format!("Fetching crate {} {} from crates.io", krate.name, krate.version));
    process::create_dir(&crates)?;
    let file = crates.join(format!("{name}.crate"));
    let mut cmd = Command::new("curl");
    cmd.args(["--proto", "=https", "--tlsv1.2", "-fsSL", "--retry", "3", "-o"])
        .arg(&file)
        .arg(&url);
    process::run(ctx, &mut cmd)?;
    let sum = process::sha256(ctx, &file)?;
    if sum != krate.sha256 {
        let _ = std::fs::remove_file(&file);
        return Err(Error::new(format!(
            "{url} has SHA-256 {sum}, not the {} the recipe pins; not using it",
            krate.sha256
        )));
    }
    // Unpacked beside its final place, then moved there: a directory
    // there is a whole crate.
    let partial = crates.join(format!("{name}.partial"));
    for old in [&dir, &partial] {
        if old.exists() {
            std::fs::remove_dir_all(old).context(|| format!("removing {}", old.display()))?;
        }
    }
    process::create_dir(&partial)?;
    let mut cmd = Command::new("tar");
    cmd.arg("-xzf").arg(&file).arg("-C").arg(&partial).arg("--no-same-owner");
    process::run(ctx, &mut cmd)?;
    let unpacked = partial.join(&name);
    if !unpacked.join("Cargo.toml").is_file() {
        return Err(Error::new(format!("{url} holds no {name}/Cargo.toml")));
    }
    write_file(&unpacked.join(".cargo-checksum.json"), &checksum)?;
    std::fs::rename(&unpacked, &dir).context(|| format!("moving it to {}", dir.display()))?;
    std::fs::remove_dir_all(&partial).context(|| format!("removing {}", partial.display()))?;
    let _ = std::fs::remove_file(&file);
    Ok(dir)
}

/// Copies the tree of `repo`'s checked-out commit, without its `.git`, to
/// `to`; with `paths`, only those directories of it.
pub fn export(ctx: &Ctx, repo: &Path, to: &Path, paths: &[String]) -> Result<()> {
    process::create_dir(to)?;
    let mut archive = Command::new("git");
    archive.arg("-C").arg(repo).args(["archive", "--format=tar", "HEAD", "--"]).args(paths);
    archive.stdout(Stdio::piped());
    if ctx.verbose {
        eprintln!("+ {} | tar -x -C {}", process::render(&archive), to.display());
    }
    let mut child = archive.spawn().context(|| "running git archive".into())?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(to)
        .stdin(stdout)
        .status()
        .context(|| "running tar".into())?;
    let archived = child.wait().context(|| "running git archive".into())?;
    if !status.success() || !archived.success() {
        return Err(Error::new(format!("copying {} to {} failed", repo.display(), to.display())));
    }
    Ok(())
}

/// Copies the directory `from`, with everything in it, to `to`, keeping
/// each file's time: make would otherwise find a release's generated
/// files, such as its configure script, older than what they are made
/// from, and make them again with programs the host need not have.
fn copy_tree(ctx: &Ctx, from: &Path, to: &Path) -> Result<()> {
    process::create_dir(to)?;
    let mut cmd = Command::new("cp");
    cmd.arg("-R").arg("-p").arg(from.join(".")).arg(to);
    process::run(ctx, &mut cmd)
}

/// Builds one port: a fresh copy of its sources with its patches applied,
/// then its build.sh.
fn build(ctx: &Ctx, toolchain: &Toolchain, port: &Port) -> Result<()> {
    let (recipe, dir) = (&port.recipe, port.dir.as_path());
    step(&format!("Building port {} {}", recipe.name, recipe.version));
    let work = ctx.target.join("ports/work").join(&recipe.name);
    if work.exists() {
        std::fs::remove_dir_all(&work).context(|| format!("removing {}", work.display()))?;
    }
    let src = work.join("src");
    let build_dir = work.join("build");
    match &port.source {
        Some(source) if recipe.source.git.is_empty() => copy_tree(ctx, source, &src)?,
        Some(source) => export(ctx, source, &src, &recipe.source.paths)?,
        None => process::create_dir(&src)?,
    }
    for (extra, source) in recipe.extra.iter().zip(&port.extras) {
        export(ctx, source, &src.join(&extra.path), &[])?;
    }
    for (krate, source) in recipe.crates.iter().zip(&port.crates) {
        copy_tree(ctx, source, &src.join(krate.path()))?;
    }
    let patches = dir.join("patches");
    if patches.is_dir() {
        let mut list: Vec<PathBuf> = std::fs::read_dir(&patches)
            .context(|| format!("reading {}", patches.display()))?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|e| e == "patch"))
            .collect();
        list.sort();
        for patch in list {
            let mut cmd = Command::new("patch");
            cmd.args(["-p1", "--batch", "--forward", "--quiet", "-d"])
                .arg(&src)
                .arg("-i")
                .arg(&patch);
            process::run(ctx, &mut cmd)?;
        }
    }
    process::create_dir(&build_dir)?;
    let info = info_dir(ctx, &recipe.name);
    if info.exists() {
        std::fs::remove_dir_all(&info).context(|| format!("removing {}", info.display()))?;
    }
    process::create_dir(&info)?;
    let sysroot = ctx.out.join("sysroot");
    process::create_dir(&sysroot)?;
    let jobs = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![toolchain.bin()];
    paths.extend(std::env::split_paths(&path));
    let tool = |name: &str| toolchain.bin().join(format!("{TRIPLE}-{name}"));
    let mut cmd = Command::new("sh");
    cmd.arg(dir.join("build.sh"))
        .current_dir(&work)
        .env(
            "PATH",
            std::env::join_paths(paths)
                .map_err(|err| Error::new(format!("building PATH: {err}")))?,
        )
        .env("PORT_DIR", dir)
        .env("PORT_SRC", &src)
        .env("PORT_BUILD", &build_dir)
        .env("PORT_INFO", &info)
        .env("SYSROOT", &sysroot)
        .env("TOOLCHAIN", &toolchain.dir)
        .env("CARGO_RADIX", toolchain.cargo())
        .env("RUST_TARGET", ctx.root.join(crate::userspace::STD_TARGET_SPEC))
        .env("PREFIX", "/system")
        .env("JOBS", jobs.to_string())
        .env("HOST", TRIPLE)
        .env("CC", toolchain.cc())
        .env("CXX", toolchain.cxx())
        .env("AR", tool("ar"))
        .env("RANLIB", tool("ranlib"))
        .env("NM", tool("nm"))
        .env("STRIP", tool("strip"))
        .env("OBJCOPY", tool("objcopy"))
        .env("CFLAGS", "-O2")
        .env("CXXFLAGS", "-O2")
        .env("CROSS_FILE", toolchain.cross_file())
        .env("CMAKE_TOOLCHAIN_FILE", toolchain.cmake_toolchain_file())
        .env("LIBC_CROSS_FILE", toolchain.libc_cross_file())
        // The host's settings must not reach a build for Radix.
        .env_remove("CPPFLAGS")
        .env_remove("LDFLAGS")
        .env_remove("PKG_CONFIG_PATH")
        .env("PKG_CONFIG_LIBDIR", sysroot.join("system/lib/pkgconfig"))
        .env("PKG_CONFIG_SYSROOT_DIR", &sysroot)
        .env("SOURCE_DATE_EPOCH", crate::image::source_date_epoch(ctx).unwrap_or("0".into()));
    process::run(ctx, &mut cmd)
        .map_err(|err| Error::new(format!("port {}: {err}", recipe.name)))?;
    // Nothing a recipe or build.sh says goes in the image may be missing.
    for file in &image_list(ctx, recipe)? {
        let path = sysroot.join(file);
        let there = if file.ends_with('/') { path.is_dir() } else { path.is_file() };
        if !there {
            return Err(Error::new(format!(
                "port {}: build.sh installed no {file} in the sysroot",
                recipe.name
            )));
        }
    }
    Ok(())
}

/// Where a port's build.sh leaves what ob needs to know of the build
/// (`$PORT_INFO`): `image`, the files it installed for the image beyond
/// those the recipe names, one path beneath the sysroot a line, such as
/// manual pages; and for a port in Rust, the crates it linked
/// (`crates.txt`, as `cargo tree --format '{p}|{l}'` lists them) and its
/// `Cargo.lock`, for the license manifest.
pub fn info_dir(ctx: &Ctx, name: &str) -> PathBuf {
    ctx.out.join("ports").join(name)
}

/// What of the sysroot a port puts in the image: what its recipe names,
/// then what its build.sh listed in `$PORT_INFO/image`.
fn image_list(ctx: &Ctx, recipe: &Recipe) -> Result<Vec<String>> {
    let mut list = recipe.image.clone();
    let listed = info_dir(ctx, &recipe.name).join("image");
    if listed.is_file() {
        let text =
            std::fs::read_to_string(&listed).context(|| format!("reading {}", listed.display()))?;
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if line.starts_with('/') || line.split('/').any(|part| part == "..") {
                return Err(Error::new(format!(
                    "port {}: `{line}` in {} is not a path beneath the sysroot",
                    recipe.name,
                    listed.display()
                )));
            }
            list.push(line.to_string());
        }
    }
    Ok(list)
}

/// The files of `recipe`'s image in `sysroot`, as (path in the image,
/// path on the host) pairs, in name order.
pub fn image_files(ctx: &Ctx, recipe: &Recipe, sysroot: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for file in &image_list(ctx, recipe)? {
        let path = sysroot.join(file);
        if !file.ends_with('/') {
            files.push((file.clone(), path));
            continue;
        }
        let mut found = Vec::new();
        list_files(&path, &mut found)?;
        found.sort();
        for host in found {
            let inside = host.strip_prefix(sysroot).unwrap_or(&host);
            files.push((inside.to_string_lossy().into_owned(), host));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_clang_versions() {
        assert_eq!(clang_major("Ubuntu clang version 20.1.2 (0ubuntu1~24.04.3)"), Some(20));
        assert_eq!(clang_major("clang version 21.0.0git"), Some(21));
        assert_eq!(clang_major("gcc (GCC) 14.2.1"), None);
    }

    #[test]
    fn recipes_parse() {
        let text = r#"
            name = "lua"
            version = "5.4.7"
            summary = "the Lua interpreter"
            license = "MIT"
            homepage = "https://www.lua.org"
            depends = ["mlibc"]
            image = ["system/bin/lua"]
            [source]
            git = "https://github.com/lua/lua.git"
            commit = "0123456789012345678901234567890123456789"
            tag = "v5.4.7"
        "#;
        let recipe: Recipe = toml::from_str(text).unwrap();
        assert_eq!(recipe.depends, ["mlibc"]);
        assert!(!recipe.toolchain);
        assert!(toml::from_str::<Recipe>(&text.replace("summary", "sumary")).is_err());
        let bundled = format!(
            "{text}\n[[bundled]]\nname = \"musl\"\nversion = \"1.2.5\"\nlicense = \"MIT\"\n\
             homepage = \"https://musl.libc.org\"\nnotices = [\"musl/COPYRIGHT\"]\n"
        );
        let recipe: Recipe = toml::from_str(&bundled).unwrap();
        assert_eq!(recipe.bundled[0].notices, ["musl/COPYRIGHT"]);
        assert_eq!(outside_path(&recipe), None);
    }

    #[test]
    fn named_paths_stay_inside() {
        let text = r#"
            name = "x"
            version = "1"
            summary = "x"
            license = "MIT"
            homepage = "https://x"
            image = ["system/bin/x", "system/share/x/"]
            [source]
            git = "https://x/x.git"
            commit = "0123456789012345678901234567890123456789"
        "#;
        let recipe: Recipe = toml::from_str(text).unwrap();
        assert_eq!(outside_path(&recipe), None);
        for (from, to) in [
            ("\"system/bin/x\"", "\"/etc/x\""),
            ("\"system/bin/x\"", "\"system/../../x\""),
            ("image = [", "notices = [\"../COPYING\"]\nimage = ["),
            (
                "[source]",
                "[[extra]]\npath = \"/tmp/x\"\ngit = \"https://y\"\ncommit = \"0123456789012345678901234567890123456789\"\nlicense = \"MIT\"\n[source]",
            ),
            ("commit = ", "paths = [\"a/../../b\"]\ncommit = "),
        ] {
            let recipe: Recipe = toml::from_str(&text.replacen(from, to, 1)).unwrap();
            assert!(outside_path(&recipe).is_some(), "{to}");
        }
    }

    #[test]
    fn archive_sources() {
        let text = r#"
            name = "grep"
            version = "3.12"
            summary = "GNU grep"
            license = "GPL-3.0-or-later"
            homepage = "https://www.gnu.org/software/grep/"
            [source]
            url = "https://ftp.gnu.org/gnu/grep/grep-3.12.tar.xz"
            sha256 = "2649b27c0e90e632eadcd757be06c6e9a4f48d941de51e7c0f83ff76408a07b9"
            mirrors = ["https://archive.ubuntu.com/ubuntu/pool/main/g/grep/grep_3.12.orig.tar.xz"]
        "#;
        let recipe: Recipe = toml::from_str(text).unwrap();
        assert_eq!(recipe.source.download(), "https://ftp.gnu.org/gnu/grep/grep-3.12.tar.xz");
        assert_eq!(recipe.source.release(), "grep-3.12.tar.xz");
        assert_eq!(recipe.source.mirrors.len(), 1);
        let git =
            Source { git: "https://x/y.git".into(), commit: "ab".into(), ..Source::default() };
        assert_eq!(git.download(), "git+https://x/y.git@ab");
        assert_eq!(git.release(), "ab");
    }
}
