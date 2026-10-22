// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Ports: software from elsewhere, built for Radix (ob(1), PORTS; ADR 0069).
//!
//! A port is a directory `ports/<name>/` with:
//!
//! - `recipe.toml`: the port's version, license and home page, the git
//!   commit its source is pinned to (or, for Rust's standard library,
//!   the pinned Rust toolchain's rust-src) and any other sources unpacked
//!   into its tree, the ports it needs built first, and the files it adds
//!   to the boot archive;
//! - `patches/*.patch`, applied in name order with `patch -p1`;
//! - `build.sh`, which builds the port in a fresh copy of its patched
//!   source and installs it into the sysroot (`out/sysroot`), or, for a
//!   port of the toolchain, into `out/toolchain`.
//!
//! Sources are fetched with git once, by commit, into `target/ports`, and
//! checked to be that commit, so a build uses nothing but what the recipe
//! names. A port is built again only when something that went into its
//! last build changed: its directory, the toolchain (and Rust's, for a
//! port of rust-src), or a port it needs.
//!
//! The toolchain is the host's Clang and LLD, [`CLANG_MIN`] or later,
//! made to target Radix by the wrapper scripts and Meson cross files ob
//! writes into `out/toolchain`: C for Radix against the C library in the
//! sysroot, linked into static position-independent programs, or, for
//! bundles that ship libraries of their own, into programs and libraries
//! on the shared C library and its dynamic linker (ADR 0071).

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
    pub source: Source,
    /// More sources, unpacked inside the port's source tree.
    #[serde(default)]
    pub extra: Vec<Extra>,
    /// Files of the sysroot the boot archive holds, as paths beneath it;
    /// one that ends in `/` is a directory, with everything in it.
    #[serde(default)]
    pub image: Vec<String>,
    /// The files of the source that hold its copyright and license
    /// notice, for a source without a LICENSE or COPYING file.
    #[serde(default)]
    pub notices: Vec<String>,
}

/// Where a source comes from: a git repository and a commit in it, or
/// rust-src.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The repository; empty for rust-src.
    #[serde(default)]
    pub git: String,
    /// The full commit ID, which pins the tree; empty for rust-src.
    #[serde(default)]
    pub commit: String,
    /// The release or tag the commit is, for people.
    #[serde(default)]
    pub tag: Option<String>,
    /// Whether the source is the `library` directory of the pinned Rust
    /// toolchain's rust-src component, with the crates it vendors: the
    /// toolchain pins it, and it needs no fetching.
    #[serde(default, rename = "rust-src")]
    pub rust_src: bool,
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

/// A port that was built: its recipe, and where its source is.
pub struct Port {
    pub recipe: Recipe,
    /// Its source as fetched, then the extra sources.
    pub sources: Vec<PathBuf>,
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
    if recipe.source.rust_src {
        if !recipe.source.git.is_empty() || !recipe.source.commit.is_empty() {
            return Err(Error::new(format!("{what}: a rust-src source has no git or commit")));
        }
        if !recipe.toolchain {
            return Err(Error::new(format!("{what}: only a toolchain port builds rust-src")));
        }
    }
    let main = (!recipe.source.rust_src).then_some(&recipe.source.commit);
    for commit in main.into_iter().chain(recipe.extra.iter().map(|e| &e.commit)) {
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new(format!("{what}: `{commit}` is not a full commit ID")));
        }
    }
    if !dir.join("build.sh").is_file() {
        return Err(Error::new(format!("{what}: the port has no build.sh")));
    }
    if recipe.toolchain && !recipe.image.is_empty() {
        return Err(Error::new(format!("{what}: a toolchain port puts nothing in the image")));
    }
    Ok(())
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

    /// Writes the wrapper scripts and cross files into `out/toolchain`.
    pub fn write(&self, ctx: &Ctx) -> Result<()> {
        process::create_dir(&self.bin())?;
        process::create_dir(&self.dir.join("meson"))?;
        let sysroot = ctx.out.join("sysroot");
        write_script(&self.cc(), &self.cc_script(&sysroot))?;
        for (name, path) in &self.tools {
            let script = format!(
                "#!/bin/sh\n# Written by ob (ob(1), PORTS): the Radix toolchain's {name}.\n\
                 exec '{}' \"$@\"\n",
                path.display()
            );
            write_script(&self.bin().join(format!("{TRIPLE}-{name}")), &script)?;
        }
        write_file(&self.libc_cross_file(), &self.libc_cross(&sysroot))?;
        write_file(&self.cross_file(), &self.programs_cross())?;
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
    fn cc_script(&self, sysroot: &Path) -> String {
        let lib = sysroot.join("system/lib");
        let mut s = String::new();
        let _ = write!(
            s,
            r#"#!/bin/sh
# Written by ob (ob(1), PORTS): the Radix toolchain's C compiler.
# {version}
link=yes
mode=static
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
	esac
	set -- "$@" "$arg"
done
set -- --target=x86_64-unknown-linux-musl \
	-U__linux__ -U__linux -Ulinux -U__gnu_linux__ \
	-D__radix__ -D__dukovo__ \
	--sysroot='{sysroot}' -nostdinc \
	-isystem '{sysroot}/system/include' -isystem '{resource}/include' \
	-fPIE -ffunction-sections -fdata-sections "$@"
if [ -n "$link" ]; then
	# The C library goes in as the program does, whichever way the
	# arguments left the linker (rustc's end with -Bdynamic).
	case $mode in
	static) start='{lib}/Scrt1.o' libc=-Bstatic; set -- -static-pie "$@" ;;
	dynamic) start='{lib}/Scrt1.o' libc=-Bdynamic
		set -- -pie -Wl,--dynamic-linker=/system/lib/ld.so -Wl,--as-needed "$@" ;;
	shared) start= libc=-Bdynamic; set -- -Wl,--as-needed "$@" ;;
	esac
	set -- -fuse-ld=lld -nostdlib -Wl,-z,max-page-size=4096 -Wl,--gc-sections \
		${{start:+"$start"}} '{lib}/crti.o' "$@" \
		-L'{lib}' -Wl,$libc -lc '{builtins}' '{lib}/crtn.o'
fi
exec '{clang}' "$@"
"#,
            version = self.version,
            sysroot = sysroot.display(),
            resource = self.resource.display(),
            lib = lib.display(),
            builtins = self.builtins.display(),
            clang = self.clang.display(),
        );
        s
    }

    /// The cross file the C library is built with: Clang for no OS, so
    /// that nothing of Linux's (ifuncs, say) leaks in; `unknown-attributes`
    /// as an error makes mlibc's ifunc check fail, as it must there. The
    /// sysroot's headers come after all others, for the kernel's
    /// (`radix/`) only: the C library has its own.
    fn libc_cross(&self, sysroot: &Path) -> String {
        // clang-20 goes with clang++-20.
        let name = self.clang.file_name().and_then(|n| n.to_str()).unwrap_or("clang");
        let clangxx = self.clang.with_file_name(name.replacen("clang", "clang++", 1));
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
             ar = '{ar}'\n\
             strip = '{strip}'\n\
             nm = '{nm}'\n\
             \n{host}",
            cc = self.cc().display(),
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
        let mut sources = vec![if recipe.source.rust_src {
            rust_src(ctx)?.0
        } else {
            fetch(ctx, &recipe.name, &recipe.source.git, &recipe.source.commit)?
        }];
        for extra in &recipe.extra {
            let name = extra.path.rsplit('/').next().unwrap_or(&extra.path);
            sources.push(fetch(ctx, name, &extra.git, &extra.commit)?);
        }
        let stamp = inputs_hash(ctx, &dir, &toolchain, &recipe, &stamps)?;
        let stamp_file = ctx.out.join("ports").join(format!("{}.stamp", recipe.name));
        let built = std::fs::read_to_string(&stamp_file).ok() == Some(format!("{stamp:016x}\n"));
        if built && !rebuild.contains(&recipe.name) {
            step(&format!("Port {} {} is up to date", recipe.name, recipe.version));
        } else {
            build(ctx, &toolchain, &recipe, &dir, &sources)?;
            // A port of Rust's standard library must agree with the C
            // library it calls.
            if recipe.source.rust_src {
                crate::libc_check::check(ctx, &toolchain)?;
            }
            process::create_dir(&ctx.out.join("ports"))?;
            write_file(&stamp_file, &format!("{stamp:016x}\n"))?;
        }
        stamps.insert(recipe.name.clone(), stamp);
        ports.push(Port { recipe, sources });
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
/// (its version, and the wrappers and cross files ob writes), the
/// kernel's system call headers, and the builds of the ports it needs.
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
    for dir in [dir, wrappers.as_path(), cross.as_path(), headers.as_path()] {
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
        let path = entry.context(|| format!("reading {}", dir.display()))?.path();
        if path.is_dir() {
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
/// and checks that it is that commit. Returns where it is.
fn fetch(ctx: &Ctx, name: &str, git: &str, commit: &str) -> Result<PathBuf> {
    let dir = ctx.target.join("ports/sources").join(format!("{name}-{}", &commit[..12]));
    let head = |dir: &Path| -> Option<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD"]).output();
        out.ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if dir.join(".git").is_dir() && head(&dir).as_deref() == Some(commit) {
        return Ok(dir);
    }
    step(&format!("Fetching {name} {} from {git}", &commit[..12]));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context(|| format!("removing {}", dir.display()))?;
    }
    process::create_dir(&dir)?;
    let git_in = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&dir).args(args);
        cmd
    };
    process::run(ctx, &mut git_in(&["init", "--quiet"]))?;
    process::run(ctx, &mut git_in(&["fetch", "--quiet", "--depth", "1", git, commit]))?;
    process::run(ctx, &mut git_in(&["checkout", "--quiet", "--detach", "FETCH_HEAD"]))?;
    match head(&dir) {
        Some(found) if found == commit => Ok(dir),
        found => Err(Error::new(format!(
            "{git} gave {} for commit {commit}",
            found.unwrap_or_else(|| "nothing".into())
        ))),
    }
}

/// Copies the tree of `repo`'s checked-out commit, without its `.git`, to
/// `to`.
fn export(ctx: &Ctx, repo: &Path, to: &Path) -> Result<()> {
    process::create_dir(to)?;
    let mut archive = Command::new("git");
    archive.arg("-C").arg(repo).args(["archive", "--format=tar", "HEAD"]).stdout(Stdio::piped());
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

/// Builds one port: a fresh copy of its source with its patches applied,
/// then its build.sh.
fn build(
    ctx: &Ctx,
    toolchain: &Toolchain,
    recipe: &Recipe,
    dir: &Path,
    sources: &[PathBuf],
) -> Result<()> {
    step(&format!("Building port {} {}", recipe.name, recipe.version));
    let work = ctx.target.join("ports/work").join(&recipe.name);
    if work.exists() {
        std::fs::remove_dir_all(&work).context(|| format!("removing {}", work.display()))?;
    }
    let src = work.join("src");
    let build_dir = work.join("build");
    if recipe.source.rust_src {
        process::create_dir(&src)?;
        let mut cmd = Command::new("cp");
        cmd.arg("-R").arg(sources[0].join(".")).arg(&src);
        process::run(ctx, &mut cmd)?;
    } else {
        export(ctx, &sources[0], &src)?;
    }
    for (extra, source) in recipe.extra.iter().zip(&sources[1..]) {
        export(ctx, source, &src.join(&extra.path))?;
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
    let sysroot = ctx.out.join("sysroot");
    process::create_dir(&sysroot)?;
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
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
        .env("SYSROOT", &sysroot)
        .env("TOOLCHAIN", &toolchain.dir)
        .env("RUST_TARGET", ctx.root.join(crate::userspace::STD_TARGET_SPEC))
        .env("PREFIX", "/system")
        .env("JOBS", jobs.to_string())
        .env("HOST", TRIPLE)
        .env("CC", toolchain.cc())
        .env("AR", tool("ar"))
        .env("RANLIB", tool("ranlib"))
        .env("NM", tool("nm"))
        .env("STRIP", tool("strip"))
        .env("OBJCOPY", tool("objcopy"))
        .env("CFLAGS", "-O2")
        .env("CROSS_FILE", toolchain.cross_file())
        .env("LIBC_CROSS_FILE", toolchain.libc_cross_file())
        // The host's settings must not reach a build for Radix.
        .env_remove("CPPFLAGS")
        .env_remove("LDFLAGS")
        .env_remove("CXXFLAGS")
        .env_remove("PKG_CONFIG_PATH")
        .env("PKG_CONFIG_LIBDIR", sysroot.join("system/lib/pkgconfig"))
        .env("PKG_CONFIG_SYSROOT_DIR", &sysroot)
        .env("SOURCE_DATE_EPOCH", crate::image::source_date_epoch(ctx).unwrap_or("0".into()));
    process::run(ctx, &mut cmd)
        .map_err(|err| Error::new(format!("port {}: {err}", recipe.name)))?;
    // Nothing a recipe says goes in the image may be missing.
    for file in &recipe.image {
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

/// The files of `recipe`'s image in `sysroot`, as (path in the image,
/// path on the host) pairs, in name order.
pub fn image_files(recipe: &Recipe, sysroot: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for file in &recipe.image {
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
    }
}
