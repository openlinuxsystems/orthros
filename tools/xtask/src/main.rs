//! `ob`, the OrthrOS build tool.
//!
//! The `./ob` shell wrapper at the top of the repository builds this program
//! with the pinned toolchain and runs it. Every command is documented in
//! ob(1); `docs/man/ob.1.scd` is the source of that page.

mod config;
mod docs;
mod doctor;
mod elf;
mod image;
mod kernel;
mod krelocs;
mod ksyms;
mod licenses;
mod lint;
mod process;
mod qemu;
mod screen;
mod test;
mod tla;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use config::{Config, Profile};

/// The OrthrOS version, shared by the kernel and every image.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every command with a one-line summary, in the order `ob help` lists them.
/// `ob docs --check` verifies that ob(1) documents each one.
pub const COMMANDS: &[(&str, &str)] = &[
    ("setup", "Install the pinned Rust toolchain, then run doctor"),
    ("doctor", "Check host tools and print the commands that install what is missing"),
    ("build", "Build the kernel, with its symbol table, into out/sysroot"),
    ("image", "Build the bootable USB image (or an ISO with --iso)"),
    ("run", "Boot the image in QEMU with the serial console on this terminal"),
    ("test", "Run unit tests, then boot the image in QEMU under UEFI and BIOS"),
    ("docs", "Build the man pages and the handbook into out/docs"),
    ("lint", "Check formatting and run Clippy with warnings denied"),
    ("fmt", "Format all Rust code"),
    ("clean", "Remove build output"),
    ("help", "Show help for ob or for one command"),
];

const USAGE: &str = "\
Usage: ./ob [--verbose] <command> [options]
       ./ob --container <command> [options]

The OrthrOS build tool. Settings come from orthros.toml; options override them.

Commands:
";

const FOOTER: &str = "
Global options:
  -v, --verbose    Print every external command before running it
  --container      Run the command inside the pinned build container
                   (Podman or Docker); handled by the ./ob wrapper
  -h, --help       Show this help, or a command's help after its name
  -V, --version    Print the OrthrOS version

Start with `./ob setup`, then `./ob run`. Full manual: ob(1), or
`man -l out/docs/man/man1/ob.1` after `./ob docs`.";

/// Per-command usage, shown by `ob help <command>` and `ob <command> --help`.
fn command_usage(name: &str) -> Option<&'static str> {
    Some(match name {
        "setup" => {
            "Usage: ./ob setup

Installs the Rust toolchain pinned in rust-toolchain.toml with rustup,
including its rust-src component (the kernel builds `core` itself) and the
aarch64-unknown-none target (CI checks the AArch64 HAL stubs), then runs
`./ob doctor`."
        }
        "doctor" => {
            "Usage: ./ob doctor

Checks every host tool the build needs and prints the exact command that
installs anything missing on Debian, Ubuntu, Fedora or Arch Linux.
Exits with status 1 when a required tool is missing."
        }
        "build" => {
            "Usage: ./ob build [--debug | --release]

Builds the Radix kernel for its own target (kernel/targets/), writes the
kernel symbol table into it, and places it in out/sysroot/boot/radix.

  --debug      Build without optimisation
  --release    Build with optimisation (the default from orthros.toml)"
        }
        "image" => {
            "Usage: ./ob image [--iso] [--cmdline <flags>] [--microcode <path>]
                  [--debug | --release]

Builds the kernel, then a hybrid boot image that starts under UEFI and
legacy BIOS from a USB stick, a hard disk or optical media:
  out/orthros-<version>-x86_64.img   (default)
  out/orthros-<version>-x86_64.iso   (with --iso)

  --iso                Name the output .iso for virtual machines and optical media
  --cmdline <flags>    Kernel flags instead of boot.cmdline (orthros-boot(7))
  --microcode <path>   Intel microcode file or directory instead of image.microcode"
        }
        "run" => {
            "Usage: ./ob run [--bios | --uefi] [--media usb|cdrom|disk] [--headless] [--gdb]
                [--cmdline <flags>] [--microcode <path>] [--debug | --release]
                [-- <extra QEMU arguments>]

Builds the image and boots it in QEMU. The serial console is this terminal;
press Ctrl-A then X to quit QEMU. KVM is used when /dev/kvm is accessible.

  --bios               Boot through SeaBIOS (legacy BIOS)
  --uefi               Boot through OVMF (UEFI)
  --media <kind>       Attach the image as a USB stick, CD-ROM or SATA disk
  --headless           Do not open a QEMU window (automatic without a display)
  --gdb                Wait for GDB on localhost:1234; writes out/qemu/radix.gdb,
                       which stops at the kernel's first Rust function (adds
                       nokaslr, so the kernel runs where its symbols say)
  --cmdline <flags>    Kernel flags instead of boot.cmdline, such as \"kdb\"
  --microcode <path>   Intel microcode file or directory instead of image.microcode"
        }
        "test" => {
            "Usage: ./ob test [unit | boot | kani | tla] [--case <name>]...
                 [--timeout <seconds>] [--debug | --release]

Runs host unit tests, then the boot tests: QEMU boots a test image under
UEFI and BIOS, from USB and CD-ROM, with 4 CPUs, and each test follows the
serial console, types kdb commands and reads the framebuffer from
screenshots. Logs go to out/test/. Give `unit` or `boot` to run only that
part; `kani` checks the Kani proof harnesses instead, and `tla` the TLA+
models with TLC (minutes each, so plain `ob test` leaves them out; TLC needs
Java, and ob downloads the pinned TLA+ tools into .cache/tla). The boot
tests are:
  uefi-usb, bios-usb, uefi-cdrom, bios-cdrom   normal boots (uefi-cdrom in xAPIC
                   mode, bios-cdrom without KASLR and PCIDs)
  screen           the boot log on the UEFI framebuffer
  page-fault       a page fault's panic report, on serial and on screen
  crash-log-uefi   the panic report survives a restart (UEFI)
  crash-log-bios   the same under BIOS
  kdb              a breakpoint and every kdb command
  mm-stress        tens of millions of heap objects on 4 CPUs and 2 NUMA nodes
  tlb              TLB shootdowns and address-space switches
  caps             no capability forged or widened; revokes against 3 CPUs
  stack-overflow   a kernel stack overflow stops at its guard page
  smep, smap       the kernel cannot run or read a user page
  microcode        a microcode module on an emulated Intel CPU

  --case <name>         Run only this boot test; repeat for more
  --timeout <seconds>   Limit for each boot test (default 300)"
        }
        "docs" => {
            "Usage: ./ob docs [--check]

Builds the man pages from docs/man/*.scd into out/docs/man (checked with
mandoc -T lint) and the handbook from docs/handbook into out/docs/handbook.

  --check      Also fail when an ob command or orthros.toml key is not
               documented in its man page"
        }
        "lint" => {
            "Usage: ./ob lint

Checks formatting with rustfmt and runs Clippy on the host tools, the kernel
(for x86-64, and for AArch64 against the HAL stubs) and the kernel
libraries, treating every warning as an error."
        }
        "fmt" => {
            "Usage: ./ob fmt

Formats every Rust source file in the repository with rustfmt."
        }
        "clean" => {
            "Usage: ./ob clean [--all]

Removes out/ and the kernel's build directory.

  --all        Also remove target/ and .cache/ (the build tool and caches)"
        }
        "help" => {
            "Usage: ./ob help [<command>]

Shows the list of commands, or the help for one command."
        }
        _ => return None,
    })
}

fn print_usage() {
    print!("{USAGE}");
    for (name, summary) in COMMANDS {
        println!("  {name:<8} {summary}");
    }
    println!("{FOOTER}");
}

/// An error with an optional hint telling the user what to do about it.
#[derive(Debug)]
pub struct Error {
    message: String,
    hint: Option<String>,
}

impl Error {
    /// Creates an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None }
    }

    /// Adds a hint, printed on its own line after the message.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Result type used throughout the build tool.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Returns early with a formatted [`Error`].
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::Error::new(format!($($arg)*)))
    };
}

/// Adds a description of what was being done to I/O errors.
pub trait Context<T> {
    /// Wraps the error as "`what`: <error>".
    fn context(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> Context<T> for io::Result<T> {
    fn context(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|err| Error::new(format!("{}: {err}", what())))
    }
}

/// Paths and settings shared by every command.
pub struct Ctx {
    /// Repository root.
    pub root: PathBuf,
    /// Build output (`out/`).
    pub out: PathBuf,
    /// Cargo output (`target/`, or `$OB_TARGET_DIR`).
    pub target: PathBuf,
    /// Contents of `orthros.toml`.
    pub config: Config,
    /// Print external commands before running them.
    pub verbose: bool,
}

impl Ctx {
    fn load(verbose: bool) -> Result<Self> {
        let root = std::env::var_os("OB_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").to_path_buf());
        let root = root
            .canonicalize()
            .context(|| format!("cannot find the repository root at {}", root.display()))?;
        let target = std::env::var_os("OB_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target"));
        let config = Config::load(&root.join("orthros.toml"))?;
        Ok(Self { out: root.join("out"), target, config, verbose, root })
    }

    /// Path relative to the repository root, for messages.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }
}

/// Command-line arguments of one command, consumed as they are recognised.
pub struct Args {
    command: &'static str,
    args: Vec<String>,
    passthrough: Vec<String>,
}

impl Args {
    fn new(command: &'static str, mut args: Vec<String>) -> Self {
        let passthrough = match args.iter().position(|arg| arg == "--") {
            Some(index) => {
                let rest = args.split_off(index);
                rest[1..].to_vec()
            }
            None => Vec::new(),
        };
        Self { command, args, passthrough }
    }

    /// Removes `name` if present and reports whether it was.
    pub fn flag(&mut self, name: &str) -> bool {
        match self.args.iter().position(|arg| arg == name) {
            Some(index) => {
                self.args.remove(index);
                true
            }
            None => false,
        }
    }

    /// Removes `name <value>` or `name=<value>` and returns the value.
    pub fn value(&mut self, name: &str) -> Result<Option<String>> {
        let prefix = format!("{name}=");
        if let Some(index) = self.args.iter().position(|arg| arg.starts_with(&prefix)) {
            return Ok(Some(self.args.remove(index)[prefix.len()..].to_string()));
        }
        match self.args.iter().position(|arg| arg == name) {
            Some(index) if index + 1 < self.args.len() => {
                self.args.remove(index);
                Ok(Some(self.args.remove(index)))
            }
            Some(_) => bail!("{name} needs a value; see `./ob help {}`", self.command),
            None => Ok(None),
        }
    }

    /// Removes and returns the first argument that is not an option.
    pub fn positional(&mut self) -> Option<String> {
        let index = self.args.iter().position(|arg| !arg.starts_with('-'))?;
        Some(self.args.remove(index))
    }

    /// Reads `--debug` / `--release`, falling back to orthros.toml.
    pub fn profile(&mut self, config: &Config) -> Result<Profile> {
        match (self.flag("--debug"), self.flag("--release")) {
            (true, true) => bail!("--debug and --release cannot be combined"),
            (true, false) => Ok(Profile::Debug),
            (false, true) => Ok(Profile::Release),
            (false, false) => Ok(config.build.profile),
        }
    }

    /// Arguments after `--`, for commands that pass them on.
    pub fn take_passthrough(&mut self) -> Vec<String> {
        std::mem::take(&mut self.passthrough)
    }

    /// Fails if any argument was not recognised.
    pub fn finish(&self) -> Result<()> {
        if let Some(arg) = self.args.first() {
            bail!(
                "unknown argument `{arg}` for `./ob {}`; see `./ob help {}`",
                self.command,
                self.command
            );
        }
        if !self.passthrough.is_empty() {
            bail!("`./ob {}` takes no arguments after `--`", self.command);
        }
        Ok(())
    }
}

fn dispatch(argv: Vec<String>) -> Result<()> {
    let mut verbose = false;
    let mut rest = argv.into_iter().peekable();
    while let Some(arg) = rest.peek() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("ob {VERSION}");
                return Ok(());
            }
            "--container" => {
                return Err(Error::new("--container is handled by the ./ob wrapper script")
                    .hint("run the command through ./ob from the repository root"));
            }
            _ => break,
        }
        rest.next();
    }
    let Some(command) = rest.next() else {
        print_usage();
        return Ok(());
    };
    let Some(&(name, _)) = COMMANDS.iter().find(|(name, _)| *name == command) else {
        return Err(Error::new(format!("unknown command `{command}`"))
            .hint("run `./ob help` for the list of commands"));
    };
    let mut args = Args::new(name, rest.collect());
    if name != "help" && (args.flag("--help") || args.flag("-h")) {
        println!("{}", command_usage(name).unwrap_or_default());
        return Ok(());
    }
    if name == "help" {
        match args.positional() {
            Some(topic) => match command_usage(&topic) {
                Some(text) => println!("{text}"),
                None => bail!("unknown command `{topic}`; run `./ob help` for the list"),
            },
            None => print_usage(),
        }
        return args.finish();
    }

    let ctx = Ctx::load(verbose)?;
    match name {
        "setup" => {
            args.finish()?;
            doctor::setup(&ctx)
        }
        "doctor" => {
            args.finish()?;
            doctor::doctor(&ctx)
        }
        "build" => {
            let profile = args.profile(&ctx.config)?;
            args.finish()?;
            kernel::build(&ctx, profile).map(|_| ())
        }
        "image" => {
            let iso = args.flag("--iso");
            let overrides = image::Overrides {
                cmdline: args.value("--cmdline")?,
                microcode: args.value("--microcode")?,
            };
            let profile = args.profile(&ctx.config)?;
            args.finish()?;
            let path = image::build_release(&ctx, profile, iso, &overrides)?;
            println!("\nImage ready: {}", ctx.display(&path));
            println!(
                "Write it to a USB stick as shown in ob(1), EXAMPLES, or boot it with ./ob run."
            );
            Ok(())
        }
        "run" => qemu::run_command(&ctx, &mut args),
        "test" => test::command(&ctx, &mut args),
        "docs" => {
            let check = args.flag("--check");
            args.finish()?;
            docs::build(&ctx, check)
        }
        "lint" => {
            args.finish()?;
            lint::lint(&ctx)
        }
        "fmt" => {
            args.finish()?;
            lint::fmt(&ctx)
        }
        "clean" => {
            let all = args.flag("--all");
            args.finish()?;
            clean(&ctx, all)
        }
        _ => unreachable!("every command in COMMANDS is dispatched"),
    }
}

fn clean(ctx: &Ctx, all: bool) -> Result<()> {
    let mut paths =
        vec![ctx.out.clone(), ctx.target.join("kernel"), ctx.root.join("kernel/target")];
    if all {
        paths.push(ctx.target.clone());
        paths.push(ctx.root.join(".cache"));
    }
    for path in paths {
        if path.exists() {
            process::step(&format!("Removing {}", ctx.display(&path)));
            std::fs::remove_dir_all(&path).context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match dispatch(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ob: error: {err}");
            if let Some(hint) = &err.hint {
                eprintln!("ob: hint: {hint}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Args {
        Args::new("run", list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parses_flags_values_and_passthrough() {
        let mut a = args(&["--bios", "--media=cdrom", "--", "-s", "-S"]);
        assert!(a.flag("--bios"));
        assert!(!a.flag("--uefi"));
        assert_eq!(a.value("--media").unwrap().as_deref(), Some("cdrom"));
        assert_eq!(a.take_passthrough(), ["-s", "-S"]);
        a.finish().unwrap();
    }

    #[test]
    fn value_may_be_separate() {
        let mut a = args(&["--timeout", "60"]);
        assert_eq!(a.value("--timeout").unwrap().as_deref(), Some("60"));
        assert!(args(&["--timeout"]).value("--timeout").is_err());
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(args(&["--frobnicate"]).finish().is_err());
    }

    #[test]
    fn every_command_has_usage() {
        for (name, _) in COMMANDS {
            assert!(command_usage(name).is_some(), "{name} has no usage text");
        }
    }

    #[test]
    fn kernel_version_matches() {
        let manifest = include_str!("../../../kernel/Cargo.toml");
        let table: toml::Table = manifest.parse().unwrap();
        let version = table["workspace"]["package"]["version"].as_str().unwrap();
        assert_eq!(version, VERSION, "kernel/Cargo.toml and Cargo.toml disagree on the version");
    }
}
