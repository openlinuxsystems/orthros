// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob test corpus`: the shell's POSIX conformance suite and its corpus of
//! real-world scripts (ADR 0103).
//!
//! `userspace/programs/nukesh/tests/corpus.toml` names the sources, each a
//! git repository pinned by its commit, and how each runs: smoosh's POSIX
//! shell tests against the results their files give, and the test suites
//! and scripts of other projects under Nukesh and under dash or bash,
//! which must give the same exit status and standard output. A difference
//! the file lists, with its reason, is expected; once it is gone, the
//! suite fails until the entry goes too, so that the list stays true.
//!
//! The scripts are other people's and run as they are written, so they
//! run in a sandbox: new user, mount, PID and network namespaces, every
//! file system read-only but a fresh `/tmp` holding a copy of the corpus
//! (nothing runs if one stays writable), a limit on processes, and each
//! test in a PID namespace of its own, which ends whole when the test
//! does or runs out of time. As root, the sandbox runs as `nobody`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, ports, userspace};

/// The corpus file, from the repository's root.
const CORPUS: &str = "userspace/programs/nukesh/tests/corpus.toml";

/// Where the corpus is inside the sandbox: on its own `/tmp`.
const INSIDE: &str = "/tmp/corpus";

/// The exit statuses of the run inside the sandbox when results differ
/// and when it fails, so that a sandbox that fails to start is told
/// apart.
const DIFFERENCES: i32 = 3;
const FAILED: i32 = 4;

/// The processes the sandbox's user may have at once.
const MAX_PROCESSES: u32 = 4096;

/// The times a test that races the machine's load is run before its
/// failure counts.
const TRIES: usize = 3;

/// The corpus file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    source: Vec<Source>,
}

/// A pinned source and how its tests or scripts run.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    /// The source's name, which `--case` selects.
    name: String,
    git: String,
    /// The full commit ID, which pins the tree.
    commit: String,
    /// The SPDX license expression of the files the corpus runs.
    license: String,
    /// The directories fetched, when not the whole tree; the files at
    /// its top come with them, as scripts often are there.
    #[serde(default)]
    paths: Vec<String>,
    kind: Kind,
    /// The directory of the tests, for `smoosh` and `urchin` sources.
    #[serde(default)]
    tests: Option<String>,
    /// The directory of the C helpers smoosh's tests run, built for the
    /// host.
    #[serde(default)]
    util: Option<String>,
    /// The shell the source is written for: the one Nukesh is compared
    /// with.
    #[serde(default)]
    shell: Dialect,
    /// More environment, beyond PATH, HOME, TERM and LC_ALL.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Directories of an `urchin` source's tests that hold none.
    #[serde(default)]
    skip: Vec<String>,
    /// The `tool` sources it needs.
    #[serde(default)]
    needs: Vec<String>,
    /// Programs its scripts start that must not run for real, whether
    /// the host has them or not, such as the servers an entrypoint
    /// starts: each is a stub that prints its name and arguments.
    #[serde(default)]
    stubs: Vec<String>,
    /// Patterns (`dir/*`) for its scripts whose `#!` line names sh or
    /// bash: each is pointed at the shell under test, as the scripts'
    /// tests would otherwise run them with the host's. A tool's are
    /// pointed at the host's shell, which it is written for.
    #[serde(default)]
    shebangs: Vec<String>,
    /// The seconds a test may take.
    #[serde(default = "default_timeout")]
    timeout: u64,
    /// The scripts of a `scripts` source.
    #[serde(default)]
    run: Vec<Run>,
    /// The tests whose results are expected to differ, and why.
    #[serde(default)]
    known: BTreeMap<String, String>,
    /// smoosh tests that race the machine's load, and why: one that
    /// fails is run again, up to [`TRIES`] times in all.
    #[serde(default)]
    timing: BTreeMap<String, String>,
}

fn default_timeout() -> u64 {
    20
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    /// smoosh's tests: each `NAME.test` with the standard output, error
    /// and exit status that `NAME.out`, `NAME.err` and `NAME.ec` give.
    Smoosh,
    /// A test tree in urchin's layout, as nvm's is: each executable file
    /// a test, with `setup` and `teardown` around each test of their
    /// directory and `setup_dir` and `teardown_dir` around them all.
    Urchin,
    /// Scripts, each run with its arguments.
    Scripts,
    /// A program the others' tests run under the host's shell, such as
    /// bats; not a test itself.
    Tool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum Dialect {
    /// POSIX sh: compared with dash, with Nukesh as `sh`.
    #[default]
    Sh,
    /// bash: compared with bash.
    Bash,
}

/// A script of a `scripts` source.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    /// The test's name; with `each`, `{file}` in it is the file's name.
    name: String,
    /// What the shell is given: the script, then its arguments.
    args: Vec<String>,
    /// A pattern (with `*`) for files in the source's top directory: a
    /// test for each.
    #[serde(default)]
    each: Option<String>,
    /// The shell, when not the source's.
    #[serde(default)]
    shell: Option<Dialect>,
    /// More environment, beyond the source's.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// The test's standard input, which is otherwise /dev/null.
    #[serde(default)]
    stdin: Option<String>,
    /// Whether the first argument is the program to run, instead of a
    /// script for the shell: a test harness, run the same way for each.
    #[serde(default)]
    exec: bool,
    /// Whether the output is TAP (`ok 1 name`, `not ok 2 name`), each
    /// line a test of its own.
    #[serde(default)]
    tap: bool,
}

/// The sources `selected` names, or every one, with the tools they need,
/// in the corpus file's order.
fn chosen<'a>(corpus: &'a Corpus, selected: &[String]) -> Vec<&'a Source> {
    let named = |source: &Source| selected.is_empty() || selected.contains(&source.name);
    let needed = |source: &Source| {
        corpus.source.iter().any(|other| named(other) && other.needs.contains(&source.name))
    };
    corpus.source.iter().filter(|source| named(source) || needed(source)).collect()
}

/// `ob test corpus`: runs the sources `selected` names, or every one.
pub fn test(ctx: &Ctx, selected: &[String]) -> Result<()> {
    let corpus = load(&ctx.root.join(CORPUS))?;
    for name in selected {
        if !corpus.source.iter().any(|source| &source.name == name) {
            let names: Vec<_> = corpus.source.iter().map(|source| source.name.as_str()).collect();
            crate::bail!("no corpus source `{name}`; the sources are {}", names.join(", "));
        }
    }
    let dash = process::find_program("dash")
        .ok_or_else(|| Error::new("dash is not installed").hint("run `./ob doctor`"))?;
    let bash = process::find_program("bash")
        .ok_or_else(|| Error::new("bash is not installed").hint("run `./ob doctor`"))?;
    for tool in ["unshare", "prlimit", "setpriv"] {
        if process::find_program(tool).is_none() {
            return Err(Error::new(format!("`{tool}` is not installed")).hint("run `./ob doctor`"));
        }
    }

    step("Building Nukesh for the host");
    process::run(ctx, &mut userspace::host_cargo(ctx, "build"))?;
    let nukesh = ctx.target.join("programs-host/debug/nukesh");

    let dir = ctx.target.join("corpus");
    let stage = dir.join("stage");
    if stage.exists() {
        std::fs::remove_dir_all(&stage).context(|| format!("removing {}", stage.display()))?;
    }
    for sub in ["bin", "shells/nukesh", "shells/reference", "stubs", "util", "src"] {
        process::create_dir(&stage.join(sub))?;
    }
    let ob = std::env::current_exe().context(|| "finding the build tool".into())?;
    process::copy(&ob, &stage.join("ob"))?;
    process::copy(&ctx.root.join(CORPUS), &stage.join("corpus.toml"))?;
    process::copy(&nukesh, &stage.join("bin/nukesh"))?;
    let links = [
        ("shells/nukesh/sh", PathBuf::from("../../bin/nukesh")),
        ("shells/nukesh/bash", PathBuf::from("../../bin/nukesh")),
        ("shells/reference/sh", dash),
        ("shells/reference/bash", bash),
    ];
    for (link, target) in links {
        std::os::unix::fs::symlink(&target, stage.join(link))
            .context(|| format!("linking {link}"))?;
    }
    for (stub, what) in [("curl", 7), ("wget", 4), ("sudo", 1)] {
        let text =
            format!("#!/bin/sh\necho \"{stub}: not in the shell corpus\" >&2\nexit {what}\n");
        write_script(&stage.join("stubs").join(stub), &text)?;
    }
    for source in chosen(&corpus, selected) {
        let repo = ports::fetch(ctx, &source.name, &source.git, &source.commit, &source.paths)?;
        let mut paths = source.paths.clone();
        if !paths.is_empty() {
            paths.extend(top_files(ctx, &repo)?);
        }
        ports::export(ctx, &repo, &stage.join("src").join(&source.name), &paths)?;
        if let Some(util) = &source.util {
            build_util(ctx, &repo.join(util), &stage.join("util"))?;
        }
    }

    step("Running the shell corpus in a sandbox");
    let work = dir.join("sandbox");
    process::create_dir(&work)?;
    let root = Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.trim_ascii() == b"0");
    let mut cmd = Command::new("unshare");
    if !root {
        cmd.args(["--user", "--map-root-user"]);
    }
    let script = SANDBOX.replace("$MAX", &MAX_PROCESSES.to_string());
    cmd.args(["--mount", "--pid", "--net", "--fork", "--mount-proc", "--", "sh", "-c", &script]);
    cmd.arg("sh").arg(if root { "root" } else { "user" }).arg(&stage).arg(&work);
    cmd.arg(format!("{INSIDE}/ob")).arg("corpus-run").arg(INSIDE).args(selected);
    let status = process::status(ctx, &mut cmd)?;
    match status.code() {
        Some(0) => Ok(()),
        Some(DIFFERENCES) => Err(Error::new("the shell corpus found differences (above)")),
        Some(FAILED) => Err(Error::new("the shell corpus stopped (above)")),
        _ => Err(Error::new(format!("the shell corpus's sandbox failed ({status})")).hint(
            "as a user, the sandbox needs user namespaces; Ubuntu 23.10 and later allow them \
             with `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`",
        )),
    }
}

/// The sandbox's set-up, run by sh as root of its new namespaces, with
/// `root` or `user` (whether it is root outside them), the stage, an
/// empty directory and the command. The corpus is copied into a new
/// file system, which then replaces `/tmp`; everything else is made
/// read-only. As root, the command runs as `nobody`, in a user namespace
/// of its own, as root of which it makes each test's PID namespace.
const SANDBOX: &str = r#"
set -eu
mode=$1 stage=$2 work=$3
shift 3
mount --make-rprivate /
mount -t tmpfs -o size=4g,mode=1777 tmpfs "$work"
cp -a "$stage" "$work/corpus"
mount --bind "$work" /tmp
if [ -d /var/tmp ]; then mount -t tmpfs -o mode=1777 tmpfs /var/tmp; fi
if [ -d /dev/shm ]; then mount -t tmpfs -o mode=1777 tmpfs /dev/shm; fi
awk '{ print $2 }' /proc/self/mounts | sort -u | while read -r point; do
    case $point in
    /proc | /proc/* | /sys | /sys/* | /dev | /dev/* | /tmp | /var/tmp) ;;
    *) mount -o remount,bind,ro "$point" 2>/dev/null || true ;;
    esac
done
# Other people's code runs only where nothing else can be written: a mount
# that is still writable stops the corpus before anything runs. Kernel
# interfaces that hold no files (nsfs, cgroup and the like) do not count.
writable=$(awk '{ for (i = 7; $i != "-"; i++) continue; type = $(i + 1) }
    $5 !~ "^/(proc|sys|dev|tmp|var/tmp)(/|$)" &&
    type !~ "^(autofs|binfmt_misc|bpf|cgroup2?|configfs|debugfs|efivarfs|fusectl|mqueue|nsfs|proc|pstore|securityfs|sysfs|tracefs)$" {
        split($6, options, ","); if (options[1] != "ro") print $5 }' /proc/self/mountinfo)
if [ -n "$writable" ]; then
    echo "ob: the sandbox could not make these read-only:" $writable >&2
    exit 1
fi
if [ "$mode" = root ]; then
    chown -R 65534:65534 /tmp/corpus
    exec setpriv --reuid=65534 --regid=65534 --clear-groups -- \
        unshare --user --map-root-user -- prlimit --nproc=$MAX -- "$@"
fi
exec prlimit --nproc=$MAX -- "$@"
"#;

fn load(path: &Path) -> Result<Corpus> {
    let text = std::fs::read_to_string(path).context(|| format!("reading {}", path.display()))?;
    let corpus: Corpus =
        toml::from_str(&text).map_err(|err| Error::new(format!("{}: {err}", path.display())))?;
    for source in &corpus.source {
        let kind_needs_tests = matches!(source.kind, Kind::Smoosh | Kind::Urchin);
        if kind_needs_tests != source.tests.is_some() {
            crate::bail!(
                "{}: `{}` needs `tests` only if it is smoosh's or urchin's",
                path.display(),
                source.name
            );
        }
        if (source.kind == Kind::Scripts) == source.run.is_empty() {
            crate::bail!(
                "{}: `{}` needs `run` only if it is a source of scripts",
                path.display(),
                source.name
            );
        }
        if source.kind != Kind::Smoosh && !source.timing.is_empty() {
            crate::bail!(
                "{}: `{}` has `timing`, which only smoosh's tests take",
                path.display(),
                source.name
            );
        }
        for need in &source.needs {
            if !corpus.source.iter().any(|other| &other.name == need && other.kind == Kind::Tool) {
                crate::bail!(
                    "{}: `{}` needs `{need}`, which is not a tool",
                    path.display(),
                    source.name
                );
            }
        }
    }
    Ok(corpus)
}

/// The files at the top of the tree of `repo`'s checked-out commit, which
/// git's cone mode checks out with any directories.
fn top_files(ctx: &Ctx, repo: &Path) -> Result<Vec<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(["ls-tree", "-z", "HEAD"]);
    let listing = process::output(ctx, &mut cmd)?.stdout;
    // Each entry is `MODE TYPE OBJECT<tab>NAME`, ended by a NUL.
    let files = listing.split(|&byte| byte == 0).filter_map(|entry| {
        let text = std::str::from_utf8(entry).ok()?;
        let (info, name) = text.split_once('\t')?;
        (info.split(' ').nth(1) == Some("blob")).then(|| name.to_string())
    });
    Ok(files.collect())
}

/// Builds the C programs in `dir` for the host into `to`.
fn build_util(ctx: &Ctx, dir: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir).context(|| format!("reading {}", dir.display()))? {
        let path = entry.context(|| format!("reading {}", dir.display()))?.path();
        if path.extension().is_some_and(|e| e == "c") {
            let name = path.file_stem().expect("a file has a name");
            // The build's Clang, or the host's C compiler.
            let compiler = ports::find_clang()
                .map(|(clang, _)| clang)
                .or_else(|| process::find_program("cc"))
                .ok_or_else(|| {
                    Error::new("no C compiler is installed").hint("run `./ob doctor`")
                })?;
            let mut cmd = Command::new(compiler);
            cmd.arg("-O2").arg("-o").arg(to.join(name)).arg(&path);
            process::run(ctx, &mut cmd)?;
        }
    }
    Ok(())
}

/// The build tool's commands for inside the sandbox, which `ob help`
/// leaves out: `corpus-run` runs the corpus, and `corpus-init` is the
/// first process of a test's PID namespace. Returns None for any other.
pub fn internal(argv: &[String]) -> Option<ExitCode> {
    match argv.first().map(String::as_str) {
        Some("corpus-run") => Some(match run_inside(Path::new(&argv[1]), &argv[2..]) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(DIFFERENCES as u8),
            Err(err) => {
                eprintln!("ob: error: {err}");
                ExitCode::from(FAILED as u8)
            }
        }),
        Some("corpus-init") => Some(init(&argv[1..])),
        _ => None,
    }
}

/// PID 1 of a test's namespace: runs the test and gives its status as
/// a shell would, 128 and the signal for one that a signal ended. When
/// it ends, the kernel ends whatever else the test left running.
fn init(command: &[String]) -> ExitCode {
    match Command::new(&command[0]).args(&command[1..]).status() {
        Ok(status) => {
            let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            ExitCode::from(u8::try_from(code).unwrap_or(255))
        }
        Err(err) => {
            eprintln!("ob: cannot run {}: {err}", command[0]);
            ExitCode::from(127)
        }
    }
}

/// A test's result.
struct Outcome {
    /// Its exit status, or None if it ran out of time.
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Outcome {
    fn status_text(&self) -> String {
        match self.status {
            Some(code) => format!("status {code}"),
            None => "ran out of time".into(),
        }
    }
}

/// A test to run: the program and its arguments, where, with what
/// environment and input.
struct Test<'a> {
    argv: Vec<String>,
    dir: &'a Path,
    env: &'a BTreeMap<String, String>,
    stdin: Option<&'a str>,
    timeout: Duration,
}

/// Runs a test in a PID namespace of its own, as `nobody` of a user
/// namespace of its own: as root, the test would read files whatever
/// their permissions.
fn run_test(stage: &Path, test: &Test) -> Outcome {
    let mut cmd = Command::new("unshare");
    cmd.args(["--map-user=65534", "--map-group=65534", "--pid", "--fork", "--kill-child"]);
    cmd.args(["--mount-proc", "--"]);
    cmd.arg(stage.join("ob")).arg("corpus-init").args(&test.argv);
    cmd.current_dir(test.dir).env_clear().envs(test.env);
    cmd.stdin(if test.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Outcome {
                status: Some(127),
                stdout: Vec::new(),
                stderr: format!("cannot run unshare: {err}").into_bytes(),
            };
        }
    };
    if let Some(text) = test.stdin {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let text = text.as_bytes().to_vec();
        std::thread::spawn(move || std::io::Write::write_all(&mut stdin, &text));
    }
    let reader = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = reader(Box::new(child.stdout.take().expect("stdout is piped")));
    let stderr = reader(Box::new(child.stderr.take().expect("stderr is piped")));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().or(status.signal().map(|s| 128 + s)),
            Ok(None) if start.elapsed() < test.timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                // unshare's end takes its child and so the namespace.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    Outcome {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    }
}

/// Inside the sandbox: runs the corpus at `stage`, the sources `selected`
/// names or every one, and reports. Returns whether every result was the
/// one expected.
fn run_inside(stage: &Path, selected: &[String]) -> Result<bool> {
    let corpus = load(&stage.join("corpus.toml"))?;
    let sources = chosen(&corpus, selected);
    for tool in sources.iter().filter(|source| source.kind == Kind::Tool) {
        let tree = stage.join("src").join(&tool.name);
        point_shebangs(&tree, &tool.shebangs, &stage.join("shells/reference"))?;
    }
    let mut problems = Vec::new();
    for source in sources.into_iter().filter(|source| source.kind != Kind::Tool) {
        let started = Instant::now();
        let (summary, differences) = match source.kind {
            Kind::Smoosh => smoosh(stage, source)?,
            Kind::Urchin | Kind::Scripts => compare(stage, source)?,
            Kind::Tool => unreachable!("tools are not run"),
        };
        for (test, why) in &differences {
            if !source.known.contains_key(test) {
                problems.push(format!("{}: {test}: {why}", source.name));
            }
        }
        for (test, reason) in &source.known {
            if !differences.contains_key(test) {
                problems.push(format!(
                    "{}: {test}: gives the expected result now, so its entry in {CORPUS} \
                     (\"{reason}\") should go",
                    source.name
                ));
            }
        }
        eprintln!(
            "==> {} ({}): {summary}; {} known differences; {} s",
            source.name,
            source.license,
            source.known.len(),
            started.elapsed().as_secs()
        );
    }
    if problems.is_empty() {
        eprintln!("==> Every result is the one expected");
        return Ok(true);
    }
    eprintln!("\n{} unexpected results:", problems.len());
    for problem in &problems {
        eprintln!("\n{problem}");
    }
    Ok(false)
}

/// What `ob test corpus` found for a source: a summary, and how each test
/// that differs from what was expected differs.
type Findings = (String, BTreeMap<String, String>);

/// smoosh's tests: Nukesh, as `sh`, must give what each test's files
/// expect. dash and bash in POSIX mode run them too, for the record.
fn smoosh(stage: &Path, source: &Source) -> Result<Findings> {
    let tests = stage.join("src").join(&source.name).join(source.tests.as_deref().unwrap_or("."));
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&tests).context(|| format!("reading {}", tests.display()))? {
        let name = entry.context(|| format!("reading {}", tests.display()))?.file_name();
        if let Some(name) = name.to_string_lossy().strip_suffix(".test") {
            names.push(name.to_string());
        }
    }
    names.sort();
    if let Some(test) = source.timing.keys().find(|test| !names.contains(test)) {
        crate::bail!("{CORPUS}: `{}` has no test {test}, which `timing` names", source.name);
    }
    let shells = [
        ("Nukesh", vec![path_text(&stage.join("shells/nukesh/sh"))]),
        ("dash", vec![path_text(&stage.join("shells/reference/sh"))]),
        ("bash --posix", vec![path_text(&stage.join("shells/reference/bash")), "--posix".into()]),
    ];
    let mut differences = BTreeMap::new();
    let mut passed = Vec::new();
    for (label, shell) in &shells {
        let mut count = 0;
        for name in &names {
            let attempt = || -> Result<Option<String>> {
                let dir = fresh_dir(&stage.join("run/smoosh"))?;
                let env = BTreeMap::from([
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                    ("HOME".into(), path_text(&dir)),
                    ("LC_ALL".into(), "C".into()),
                    ("TEST_SHELL".into(), shell[0].clone()),
                    ("TEST_UTIL".into(), path_text(&stage.join("util"))),
                ]);
                let mut argv = shell.clone();
                argv.push(path_text(&tests.join(format!("{name}.test"))));
                let test = Test {
                    argv,
                    dir: &dir,
                    env: &env,
                    stdin: None,
                    timeout: Duration::from_secs(source.timeout),
                };
                smoosh_verdict(&tests.join(name), &run_test(stage, &test))
            };
            let mut verdict = attempt()?;
            let tries = if source.timing.contains_key(name) { TRIES } else { 1 };
            for _ in 1..tries {
                if verdict.is_none() {
                    break;
                }
                verdict = attempt()?;
            }
            match verdict {
                None => count += 1,
                Some(why) if *label == "Nukesh" => {
                    differences.insert(name.clone(), why);
                }
                Some(_) => {}
            }
        }
        passed.push(format!("{label} {count}"));
    }
    Ok((format!("{} tests; passed by {}", names.len(), passed.join(", ")), differences))
}

/// How a smoosh test's outcome differs from what its files expect, if it
/// does: `base` and `.out`, `.err` and `.ec`, each there or not.
fn smoosh_verdict(base: &Path, outcome: &Outcome) -> Result<Option<String>> {
    let read = |extension: &str| -> Result<Option<Vec<u8>>> {
        let path = PathBuf::from(format!("{}.{extension}", base.display()));
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::new(format!("reading {}: {err}", path.display()))),
        }
    };
    let status = match read("ec")? {
        Some(text) if !text.trim_ascii().is_empty() => {
            String::from_utf8_lossy(&text).trim().parse::<i32>().unwrap_or(-1)
        }
        _ => 0,
    };
    let mut why = Vec::new();
    if outcome.status != Some(status) {
        why.push(format!("status {status} expected, {}", outcome.status_text()));
    }
    if let Some(expected) = read("out")?
        && expected != outcome.stdout
    {
        why.push(first_difference(
            "standard output",
            "the test expects",
            &expected,
            &outcome.stdout,
        ));
    }
    if let Some(expected) = read("err")?
        && expected != outcome.stderr
    {
        why.push(first_difference(
            "standard error",
            "the test expects",
            &expected,
            &outcome.stderr,
        ));
    }
    Ok((!why.is_empty()).then(|| why.join("; ")))
}

/// A project's tests or scripts under the shell they are written for and
/// under Nukesh: each must give the same exit status and standard output.
fn compare(stage: &Path, source: &Source) -> Result<Findings> {
    let reference = match source.shell {
        Dialect::Sh => "dash",
        Dialect::Bash => "bash",
    };
    let mut results: [BTreeMap<String, Outcome>; 2] = [BTreeMap::new(), BTreeMap::new()];
    for (index, shells) in ["reference", "nukesh"].into_iter().enumerate() {
        // Each shell has a fresh copy of the tree in the same place, so
        // that the paths in what the tests print are the same.
        let tree = stage.join("run").join(&source.name);
        if tree.exists() {
            std::fs::remove_dir_all(&tree).context(|| format!("removing {}", tree.display()))?;
        }
        process::create_dir(&stage.join("run"))?;
        let mut copy = Command::new("cp");
        copy.arg("-a").arg(stage.join("src").join(&source.name)).arg(&tree);
        let copied = copy.status().context(|| "running cp".into())?;
        if !copied.success() {
            crate::bail!("copying {} failed", source.name);
        }
        let bin = stage.join("shells").join(shells);
        point_shebangs(&tree, &source.shebangs, &bin)?;
        let stubs = fresh_dir(&stage.join("run/stubs"))?;
        for stub in &source.stubs {
            let text = format!("#!/bin/sh\necho \"{stub}, a stub of the shell corpus:\" \"$@\"\n");
            write_script(&stubs.join(stub), &text)?;
        }
        let home = fresh_dir(&stage.join("run/home"))?;
        let fill = |text: &str| {
            let mut text = text
                .replace("{src}", &path_text(&tree))
                .replace("{home}", &path_text(&home))
                .replace("{bash}", &path_text(&stage.join("shells/reference/bash")));
            for tool in &source.needs {
                text = text.replace(
                    &format!("{{tool:{tool}}}"),
                    &path_text(&stage.join("src").join(tool)),
                );
            }
            text
        };
        let mut env = BTreeMap::from([
            (
                "PATH".to_string(),
                format!(
                    "{}:{}:{}/stubs:/usr/bin:/bin",
                    bin.display(),
                    stubs.display(),
                    stage.display()
                ),
            ),
            ("HOME".into(), path_text(&home)),
            ("TERM".into(), "dumb".into()),
            ("LC_ALL".into(), "C".into()),
        ]);
        env.extend(source.env.iter().map(|(name, value)| (name.clone(), fill(value))));
        let timeout = Duration::from_secs(source.timeout);
        let shell_for = |dialect: Dialect| {
            path_text(&bin.join(match dialect {
                Dialect::Sh => "sh",
                Dialect::Bash => "bash",
            }))
        };
        let found = &mut results[index];
        if source.kind == Kind::Urchin {
            let tests = tree.join(source.tests.as_deref().unwrap_or("."));
            let walk = Walk { stage, source, shell: shell_for(source.shell), env: &env, timeout };
            walk.dir(&tests, "", found)?;
            continue;
        }
        for run in &source.run {
            let mut env = env.clone();
            env.extend(run.env.iter().map(|(name, value)| (name.clone(), fill(value))));
            for (name, file) in each(run, &tree)? {
                let mut argv = Vec::new();
                if !run.exec {
                    argv.push(shell_for(run.shell.unwrap_or(source.shell)));
                }
                argv.extend(run.args.iter().map(|arg| fill(arg).replace("{file}", &file)));
                let test =
                    Test { argv, dir: &tree, env: &env, stdin: run.stdin.as_deref(), timeout };
                let outcome = run_test(stage, &test);
                if run.tap {
                    found.extend(tap_tests(&name, outcome));
                } else {
                    found.insert(name, outcome);
                }
            }
        }
    }
    let [expected, got] = results;
    let mut differences = BTreeMap::new();
    let mut passed = 0;
    for (name, want) in &expected {
        let Some(outcome) = got.get(name) else {
            differences.insert(name.clone(), format!("{reference} ran it, Nukesh did not"));
            continue;
        };
        let mut why = Vec::new();
        if want.status != outcome.status {
            why.push(format!(
                "{reference} gives {}, Nukesh {}",
                want.status_text(),
                outcome.status_text()
            ));
        }
        if want.stdout != outcome.stdout {
            why.push(first_difference(
                "standard output",
                &format!("{reference} gives"),
                &want.stdout,
                &outcome.stdout,
            ));
        }
        if why.is_empty() {
            passed += usize::from(want.status == Some(0));
            continue;
        }
        let mut why = why.join("; ");
        let tail = last_lines(&outcome.stderr, 6);
        if !tail.is_empty() {
            let _ =
                write!(why, "\n    Nukesh's standard error ends:\n      {}", tail.join("\n      "));
        }
        differences.insert(name.clone(), why);
    }
    let same = expected.len() - differences.len();
    let summary = format!(
        "{} tests; {same} give what {reference} gives ({passed} of them succeed)",
        expected.len()
    );
    Ok((summary, differences))
}

/// A TAP run's tests: `name` itself, with the run's status and its plan
/// lines (`1..179`), and each test it reports, named after it, with
/// status 0 or 1 and the lines of comment after it as its standard error.
/// The comments name temporary files, which differ from run to run, so
/// they are only shown, not compared.
fn tap_tests(name: &str, outcome: Outcome) -> Vec<(String, Outcome)> {
    let text = String::from_utf8_lossy(&outcome.stdout).into_owned();
    let mut plan = Vec::new();
    let mut tests: Vec<(String, Outcome)> = Vec::new();
    for line in text.lines() {
        let result = match line.strip_prefix("not ok ") {
            Some(rest) => Some((1, rest)),
            None => line.strip_prefix("ok ").map(|rest| (0, rest)),
        };
        if let Some((status, rest)) = result {
            let title = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
            let test = Outcome { status: Some(status), stdout: Vec::new(), stderr: Vec::new() };
            tests.push((format!("{name}: {title}"), test));
        } else if line.starts_with("1..") {
            plan.extend_from_slice(line.as_bytes());
            plan.push(b'\n');
        } else if let Some((_, test)) = tests.last_mut() {
            test.stderr.extend_from_slice(line.as_bytes());
            test.stderr.push(b'\n');
        }
    }
    let run = Outcome { status: outcome.status, stdout: plan, stderr: outcome.stderr };
    tests.insert(0, (name.to_string(), run));
    tests
}

/// Points the `#!` lines of `tree`'s scripts that `patterns` match
/// (`dir/*`) and that name sh or bash at those in `bin`, keeping any
/// arguments: `#!/usr/bin/env bash` becomes `#!BIN/bash`.
fn point_shebangs(tree: &Path, patterns: &[String], bin: &Path) -> Result<()> {
    for pattern in patterns {
        let (dir, files) = pattern.rsplit_once('/').unwrap_or((".", pattern));
        let dir = tree.join(dir);
        for entry in std::fs::read_dir(&dir).context(|| format!("reading {}", dir.display()))? {
            let path = entry.context(|| format!("reading {}", dir.display()))?.path();
            let name =
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !path.is_file() || !matches(files, &name) {
                continue;
            }
            let bytes = std::fs::read(&path).context(|| format!("reading {}", path.display()))?;
            let end = bytes.iter().position(|&b| b == b'\n').unwrap_or(bytes.len());
            let Some(line) = bytes[..end].strip_prefix(b"#!") else { continue };
            let line = String::from_utf8_lossy(line);
            let mut words: Vec<&str> = line.split_whitespace().collect();
            if words.first() == Some(&"/usr/bin/env") {
                words.remove(0);
            }
            let Some(program) = words.first().map(|w| w.rsplit('/').next().unwrap_or(w)) else {
                continue;
            };
            if program != "sh" && program != "bash" {
                continue;
            }
            let mut text = format!("#!{}", bin.join(program).display());
            for word in &words[1..] {
                text.push(' ');
                text.push_str(word);
            }
            let mut out = text.into_bytes();
            out.extend_from_slice(&bytes[end..]);
            std::fs::write(&path, out).context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// The tests of a `scripts` source's run: one, or with `each`, one for
/// each file in `tree` that the pattern matches, with its name and file.
fn each(run: &Run, tree: &Path) -> Result<Vec<(String, String)>> {
    let Some(pattern) = &run.each else {
        return Ok(vec![(run.name.clone(), String::new())]);
    };
    let mut files = Vec::new();
    for entry in std::fs::read_dir(tree).context(|| format!("reading {}", tree.display()))? {
        let name = entry.context(|| format!("reading {}", tree.display()))?.file_name();
        let name = name.to_string_lossy().into_owned();
        if matches(pattern, &name) {
            files.push((run.name.replace("{file}", &name), name));
        }
    }
    files.sort();
    if files.is_empty() {
        crate::bail!("no file in {} matches `{pattern}`", tree.display());
    }
    Ok(files)
}

/// Whether `name` matches `pattern`, in which `*` stands for any text.
fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, rest)) => {
            name.starts_with(head)
                && (0..=name.len() - head.len())
                    .filter(|&skip| name.is_char_boundary(head.len() + skip))
                    .any(|skip| matches(rest, &name[head.len() + skip..]))
        }
    }
}

/// The names of the files that run around an urchin test tree's tests.
const URCHIN_SPECIAL: &[&str] = &["setup", "setup_dir", "teardown", "teardown_dir"];

/// A walk through a test tree in urchin's layout, under one shell.
struct Walk<'a> {
    stage: &'a Path,
    source: &'a Source,
    shell: String,
    env: &'a BTreeMap<String, String>,
    timeout: Duration,
}

impl Walk<'_> {
    /// Runs the tests of `dir`, which is `rel` in the tree, and those of
    /// the directories beneath it, in the order of their names, into
    /// `found`.
    fn dir(&self, dir: &Path, rel: &str, found: &mut BTreeMap<String, Outcome>) -> Result<()> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir).context(|| format!("reading {}", dir.display()))? {
            let entry = entry.context(|| format!("reading {}", dir.display()))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        self.special(dir, "setup_dir");
        for name in &names {
            let path = dir.join(name);
            let rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if path.is_dir() {
                if !self.source.skip.contains(name) {
                    self.dir(&path, &rel, found)?;
                }
            } else if !URCHIN_SPECIAL.contains(&name.as_str()) && is_executable(&path) {
                self.special(dir, "setup");
                found.insert(rel, self.run(dir, name));
                self.special(dir, "teardown");
            }
        }
        self.special(dir, "teardown_dir");
        Ok(())
    }

    /// Runs `dir`'s `name` (`setup`, say) if it has one.
    fn special(&self, dir: &Path, name: &str) {
        if dir.join(name).is_file() {
            self.run(dir, name);
        }
    }

    fn run(&self, dir: &Path, name: &str) -> Outcome {
        let test = Test {
            argv: vec![self.shell.clone(), name.to_string()],
            dir,
            env: self.env,
            stdin: None,
            timeout: self.timeout,
        };
        run_test(self.stage, &test)
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

/// An empty directory at `path`, emptied if it was there.
/// Writes an executable script.
fn write_script(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .context(|| format!("making {} executable", path.display()))
}

fn fresh_dir(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        std::fs::remove_dir_all(path).context(|| format!("removing {}", path.display()))?;
    }
    process::create_dir(path)?;
    Ok(path.to_path_buf())
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Where `got` first differs from `expected`, by line, for a report.
fn first_difference(what: &str, expected_by: &str, expected: &[u8], got: &[u8]) -> String {
    let expected = String::from_utf8_lossy(expected);
    let got = String::from_utf8_lossy(got);
    let (expected, got): (Vec<_>, Vec<_>) = (expected.lines().collect(), got.lines().collect());
    let line = expected
        .iter()
        .zip(&got)
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(got.len()));
    format!(
        "{what} differs at line {}: {expected_by} {:?}, Nukesh gives {:?}",
        line + 1,
        expected.get(line).copied().unwrap_or("(the end)"),
        got.get(line).copied().unwrap_or("(the end)")
    )
}

/// The last `count` lines of `bytes` that are not empty.
fn last_lines(bytes: &[u8], count: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<_> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(count)..].iter().map(|line| line.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_as_globs() {
        assert!(matches("shunit2_*_test.sh", "shunit2_asserts_test.sh"));
        assert!(matches("*", ""));
        assert!(matches("a*b*c", "aXbYc"));
        assert!(!matches("shunit2_*_test.sh", "shunit2_test_helpers"));
        assert!(!matches("a*b", "ab c"));
    }

    #[test]
    fn the_corpus_file_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(CORPUS);
        let corpus = load(&path).unwrap();
        assert!(corpus.source.iter().any(|source| source.kind == Kind::Smoosh));
        for source in &corpus.source {
            assert_eq!(source.commit.len(), 40, "{}: a full commit ID", source.name);
        }
    }

    #[test]
    fn differences_name_the_line() {
        let why = first_difference("standard output", "dash gives", b"a\nb\n", b"a\nc\n");
        assert_eq!(why, "standard output differs at line 2: dash gives \"b\", Nukesh gives \"c\"");
        let why = first_difference("standard output", "dash gives", b"a\n", b"a\nc\n");
        assert_eq!(
            why,
            "standard output differs at line 2: dash gives \"(the end)\", Nukesh gives \"c\""
        );
    }
}
