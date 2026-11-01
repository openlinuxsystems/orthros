// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `ob docs`: man pages and the handbook.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::process::{self, step};
use crate::{COMMANDS, Context, Ctx, Error, Result, config, test, userspace};

/// A man page source such as `docs/man/dukovo.toml.5.scd`.
#[derive(Debug, PartialEq, Eq)]
struct Page {
    name: String,
    section: String,
    source: PathBuf,
}

impl Page {
    /// Splits `<name>.<section>.scd`.
    fn from_path(path: &Path) -> Option<Self> {
        let file = path.file_name()?.to_str()?.strip_suffix(".scd")?;
        let (name, section) = file.rsplit_once('.')?;
        let valid = !name.is_empty() && section.starts_with(|c: char| ('1'..='9').contains(&c));
        valid.then(|| Self { name: name.into(), section: section.into(), source: path.into() })
    }

    fn output(&self, man_dir: &Path) -> PathBuf {
        man_dir
            .join(format!("man{}", &self.section[..1]))
            .join(format!("{}.{}", self.name, self.section))
    }
}

fn pages(ctx: &Ctx) -> Result<Vec<Page>> {
    let dir = ctx.root.join("docs/man");
    let mut pages = Vec::new();
    for entry in std::fs::read_dir(&dir).context(|| format!("reading {}", dir.display()))? {
        let path = entry.context(|| format!("reading {}", dir.display()))?.path();
        if path.extension().is_some_and(|ext| ext == "scd") {
            match Page::from_path(&path) {
                Some(page) => pages.push(page),
                None => crate::bail!("{} is not named <name>.<section>.scd", ctx.display(&path)),
            }
        }
    }
    pages.sort_by(|a, b| a.source.cmp(&b.source));
    Ok(pages)
}

/// The man(2) pages ridlc generates from `abi/*.ridl`, written as scdoc
/// into `out/docs/man-src/`.
fn generated_pages(ctx: &Ctx) -> Result<Vec<Page>> {
    let dir = ctx.out.join("docs/man-src");
    process::create_dir(&dir)?;
    let mut pages = Vec::new();
    for (name, text) in ridlc::man_pages(&userspace::abi(ctx)?) {
        let path = dir.join(&name);
        std::fs::write(&path, text).context(|| format!("writing {}", path.display()))?;
        match Page::from_path(&path) {
            Some(page) => pages.push(page),
            None => crate::bail!("ridlc named a man page {name}, not <name>.<section>.scd"),
        }
    }
    Ok(pages)
}

/// dukovo-boot-tests(7), made from the `//!` lines of the boot tests in
/// tools/xtask/src/test/cases, written as scdoc into `out/docs/man-src/`.
fn boot_tests_page(ctx: &Ctx) -> Result<Page> {
    let mut text = String::from(
        "dukovo-boot-tests(7) \"Dukovo\" \"Dukovo Manual\"\n\n\
         # NAME\n\n\
         dukovo-boot-tests - the boot tests that have a file each\n\n\
         # DESCRIPTION\n\n\
         *ob test* runs these after those *ob*(1) describes. Each is a file of\n\
         _tools/xtask/src/test/cases_, whose first lines this page is made from.\n\n",
    );
    for (case, doc) in test::described_cases() {
        text.push_str(&format!("*{}*\n", case.name));
        for line in doc.lines() {
            if line.is_empty() {
                text.push('\n');
            } else {
                text.push_str(&format!("\t{line}\n"));
            }
        }
        text.push('\n');
    }
    text.push_str("# SEE ALSO\n\n*ob*(1)\n");
    let dir = ctx.out.join("docs/man-src");
    process::create_dir(&dir)?;
    let path = dir.join("dukovo-boot-tests.7.scd");
    std::fs::write(&path, text).context(|| format!("writing {}", path.display()))?;
    Ok(Page::from_path(&path).expect("named <name>.<section>.scd"))
}

/// `ob docs [--check]`.
pub fn build(ctx: &Ctx, check: bool) -> Result<()> {
    for tool in ["scdoc", "mandoc", "mdbook"] {
        if process::find_program(tool).is_none() {
            return Err(Error::new(format!("{tool} is not installed"))
                .hint("run `./ob doctor` to see what is missing"));
        }
    }
    let mut pages = pages(ctx)?;
    let mut generated = generated_pages(ctx)?;
    generated.push(boot_tests_page(ctx)?);
    let man_dir = ctx.out.join("docs/man");
    step(&format!(
        "Building {} man pages, {} of them generated from abi/*.ridl",
        pages.len() + generated.len(),
        generated.len()
    ));
    pages.extend(generated);
    let mut problems = Vec::new();
    for page in &pages {
        let output = page.output(&man_dir);
        if let Err(err) = compile(ctx, page, &output) {
            problems.push(format!("{}: {err}", ctx.display(&page.source)));
            continue;
        }
        problems.extend(lint_page(ctx, &output)?);
        if check {
            problems.extend(check_title(ctx, page)?);
        }
    }

    if check {
        problems.extend(check_coverage(ctx)?);
    }

    let (index, adr_problems) = adr_index(&read_adrs(&ctx.root.join("docs/adr"))?);
    problems.extend(adr_problems);
    let index_path = ctx.out.join("docs/adr/index.md");
    process::create_dir(&ctx.out.join("docs/adr"))?;
    std::fs::write(&index_path, index).context(|| format!("writing {}", index_path.display()))?;

    step("Building the handbook");
    let handbook_out = ctx.out.join("docs/handbook");
    let mut cmd = Command::new("mdbook");
    cmd.arg("build").arg(ctx.root.join("docs/handbook")).arg("--dest-dir").arg(&handbook_out);
    process::run(ctx, &mut cmd)?;

    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("    {problem}");
        }
        crate::bail!("{} documentation problem(s)", problems.len());
    }
    step(&format!(
        "Docs ready: {} and {}/index.html",
        ctx.display(&man_dir),
        ctx.display(&handbook_out)
    ));
    eprintln!("    Read a page with: man -l {}", ctx.display(&man_dir.join("man1/ob.1")));
    Ok(())
}

/// Compiles `page` with scdoc into `output`.
fn compile(ctx: &Ctx, page: &Page, output: &Path) -> Result<()> {
    process::create_dir(output.parent().expect("man page has a section directory"))?;
    let input = std::fs::File::open(&page.source)
        .context(|| format!("opening {}", page.source.display()))?;
    let out_file =
        std::fs::File::create(output).context(|| format!("creating {}", output.display()))?;
    let mut cmd = Command::new("scdoc");
    cmd.stdin(input).stdout(out_file).stderr(Stdio::piped());
    process::output(ctx, &mut cmd).map(|_| ())
}

/// The pages of what runs on the build host, which the image leaves out.
const HOST_PAGES: &[&str] = &["ob.1", "dukovo.toml.5"];

/// Compiles Dukovo's own manual pages, and those generated from
/// `abi/*.ridl`, into `out/sysroot/system/resources/man` for the image, but
/// not the build host's ([`HOST_PAGES`]), and returns each one's place in
/// the boot archive and file.
pub fn image_pages(ctx: &Ctx) -> Result<Vec<(String, PathBuf)>> {
    if process::find_program("scdoc").is_none() {
        return Err(
            Error::new("scdoc is not installed").hint("run `./ob doctor` to see what is missing")
        );
    }
    let mut pages = pages(ctx)?;
    pages.extend(generated_pages(ctx)?);
    let sysroot = ctx.out.join("sysroot");
    let man_dir = sysroot.join(crate::manuals::MAN_DIR);
    let mut files = Vec::new();
    for page in &pages {
        if HOST_PAGES.contains(&format!("{}.{}", page.name, page.section).as_str()) {
            continue;
        }
        let output = page.output(&man_dir);
        compile(ctx, page, &output)
            .map_err(|err| Error::new(format!("compiling {}: {err}", ctx.display(&page.source))))?;
        let path = output.strip_prefix(&sysroot).expect("the page is in the sysroot");
        files.push((path.to_string_lossy().into_owned(), output));
    }
    Ok(files)
}

/// Runs `mandoc -T lint` and reports warnings and errors.
///
/// scdoc emits a `.PP` after every heading and blank line, which mandoc
/// reports as "skipping paragraph macro". Those are artefacts of the
/// generator, not of the page, so they are not counted.
fn lint_page(ctx: &Ctx, page: &Path) -> Result<Vec<String>> {
    let mut cmd = Command::new("mandoc");
    cmd.args(["-T", "lint", "-W", "warning"]).arg(page);
    if ctx.verbose {
        eprintln!("+ {}", process::render(&cmd));
    }
    let out = cmd.output().context(|| "running mandoc".to_string())?;
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.contains("skipping paragraph macro"))
        .map(str::to_string)
        .collect())
}

/// The first line of an scdoc page must be `<name>(<section>)`.
fn check_title(ctx: &Ctx, page: &Page) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(&page.source)
        .context(|| format!("reading {}", page.source.display()))?;
    let expected = format!("{}({})", page.name, page.section);
    let first = text.lines().next().unwrap_or_default();
    if first.split_whitespace().next() == Some(expected.as_str()) {
        Ok(Vec::new())
    } else {
        Ok(vec![format!("{}: first line should start with {expected}", ctx.display(&page.source))])
    }
}

/// Every `ob` command, boot test, dukovo.toml key, kernel flag, kdb
/// command and self-test, service file setting, and bundle manifest field
/// and permission must be documented, in bold, in its man page.
fn check_coverage(ctx: &Ctx) -> Result<Vec<String>> {
    let ob_commands: Vec<&str> = COMMANDS.iter().map(|&(name, _)| name).collect();
    // The tests in test/cases are described by their files instead.
    let own: Vec<&str> = test::described_cases().map(|(case, _)| case.name).collect();
    let boot_tests: Vec<&str> =
        test::all_cases().map(|case| case.name).filter(|name| !own.contains(name)).collect();
    let kernel_flags: Vec<&str> = radix_hal::cmdline::FLAGS.iter().map(|f| f.name).collect();
    let kdb_commands: Vec<&str> = radix_kdb::COMMANDS.iter().map(|c| c.name).collect();
    let permissions: Vec<&str> =
        manifest::bundle::permissions::LIST.iter().map(|p| p.name).collect();
    let lists: [(&str, &str, &[&str]); 10] = [
        ("docs/man/ob.1.scd", "command", &ob_commands),
        ("docs/man/ob.1.scd", "boot test", &boot_tests),
        ("docs/man/dukovo.toml.5.scd", "key", config::KEYS),
        ("docs/man/service.toml.5.scd", "setting", manifest::service::KEYS),
        ("docs/man/manifest.json.5.scd", "field", manifest::bundle::KEYS),
        ("docs/man/manifest.json.5.scd", "reserved field", manifest::bundle::RESERVED),
        ("docs/man/manifest.json.5.scd", "permission", &permissions),
        ("docs/man/dukovo-boot.7.scd", "kernel flag", &kernel_flags),
        ("docs/man/radix-debug.7.scd", "kdb command", &kdb_commands),
        ("docs/man/radix-debug.7.scd", "self-test", radix_hal::cmdline::SELFTESTS),
    ];
    let mut problems = Vec::new();
    for (page, what, names) in lists {
        let text = read(ctx, page)?;
        for name in undocumented(&text, names) {
            problems.push(format!("{page} does not document the {what} `{name}`"));
        }
    }
    Ok(problems)
}

/// Each decision record in `dir` (docs/adr): its file name and text, in
/// the order of their names.
fn read_adrs(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut adrs = Vec::new();
    for entry in std::fs::read_dir(dir).context(|| format!("reading {}", dir.display()))? {
        let path = entry.context(|| format!("reading {}", dir.display()))?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let numbered = name.as_bytes().first().is_some_and(u8::is_ascii_digit);
        if numbered && path.extension().is_some_and(|ext| ext == "md") {
            let text = std::fs::read_to_string(&path).context(|| format!("reading {name}"))?;
            adrs.push((name.to_string(), text));
        }
    }
    adrs.sort();
    Ok(adrs)
}

/// The index of the decision records, made from their files so that no
/// pull request edits a shared table (threads that build phases in
/// parallel added rows at the same place, and conflicted), and the
/// problems with them: a number two records take, or a title that does
/// not start with its file's number.
fn adr_index(adrs: &[(String, String)]) -> (String, Vec<String>) {
    let mut index = String::from(
        "# Architecture decision records\n\n| Number | Decision | Status |\n| --- | --- | --- |\n",
    );
    let mut problems = Vec::new();
    let mut previous = "";
    for (name, text) in adrs {
        let number = name.split('-').next().unwrap_or_default();
        if number.len() != 4 || !number.bytes().all(|b| b.is_ascii_digit()) {
            problems.push(format!("docs/adr/{name} is not named NNNN-title.md"));
            continue;
        }
        if number == previous {
            problems
                .push(format!("docs/adr/{name} takes number {number}, which another record has"));
        }
        previous = number;
        let heading = text.lines().find_map(|line| line.strip_prefix("# ")).unwrap_or_default();
        let title = match heading.strip_prefix(&format!("{number}. ")) {
            Some(title) => title,
            None if number == "0000" => heading,
            None => {
                problems.push(format!("docs/adr/{name}'s title does not start with `{number}. `"));
                heading
            }
        };
        let status = text
            .lines()
            .find_map(|line| line.strip_prefix("- Status: "))
            .and_then(|status| status.split(';').next())
            .unwrap_or_default();
        index.push_str(&format!("| [{number}]({name}) | {title} | {status} |\n"));
    }
    (index, problems)
}

/// The names that do not appear in bold (`*name*`) in `page`.
fn undocumented<'a>(page: &str, names: &[&'a str]) -> Vec<&'a str> {
    names.iter().copied().filter(|name| !page.contains(&format!("*{name}*"))).collect()
}

fn read(ctx: &Ctx, relative: &str) -> Result<String> {
    let path = ctx.root.join(relative);
    std::fs::read_to_string(&path).context(|| format!("reading {relative}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_page_names() {
        let page = Page::from_path(Path::new("docs/man/dukovo.toml.5.scd")).unwrap();
        assert_eq!((page.name.as_str(), page.section.as_str()), ("dukovo.toml", "5"));
        assert_eq!(page.output(Path::new("out")), Path::new("out/man5/dukovo.toml.5"));
        assert!(Page::from_path(Path::new("docs/man/README.scd")).is_none());
        assert!(Page::from_path(Path::new("docs/man/ob.x.scd")).is_none());
    }

    #[test]
    fn finds_undocumented_names() {
        let page = "*kdb*\n\tstart the debugger\n*trace* [*on*|*off*]";
        assert_eq!(undocumented(page, &["kdb", "trace", "on", "regs"]), ["regs"]);
    }

    #[test]
    fn indexes_decision_records_and_finds_clashes() {
        let adr = |name: &str, text: &str| (name.to_string(), text.to_string());
        let (index, problems) = adr_index(&[
            adr("0001-a.md", "# 0001. A\n\n- Status: Accepted; changed by 0002\n"),
            adr("0002-b.md", "# 0002. B\n\n- Status: Accepted\n"),
            adr("0002-c.md", "# 0003. C\n"),
        ]);
        assert!(index.contains("| [0001](0001-a.md) | A | Accepted |\n"));
        assert_eq!(
            problems,
            [
                "docs/adr/0002-c.md takes number 0002, which another record has",
                "docs/adr/0002-c.md's title does not start with `0002. `",
            ]
        );
    }

    /// The records in the tree, so that `cargo test` finds a clash of
    /// numbers between pull requests before the docs job does.
    #[test]
    fn decision_records_have_numbers_of_their_own() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/adr");
        let adrs = read_adrs(&dir).unwrap();
        let (_, problems) = adr_index(&adrs);
        assert!(problems.is_empty(), "{problems:#?}");
    }
}
