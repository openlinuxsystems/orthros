//! `ob docs`: man pages and the handbook.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::process::{self, step};
use crate::{COMMANDS, Context, Ctx, Error, Result, config};

/// A man page source such as `docs/man/orthros.toml.5.scd`.
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

/// `ob docs [--check]`.
pub fn build(ctx: &Ctx, check: bool) -> Result<()> {
    for tool in ["scdoc", "mandoc", "mdbook"] {
        if process::find_program(tool).is_none() {
            return Err(Error::new(format!("{tool} is not installed"))
                .hint("run `./ob doctor` to see what is missing"));
        }
    }
    let pages = pages(ctx)?;
    let man_dir = ctx.out.join("docs/man");
    step(&format!("Building {} man pages", pages.len()));
    let mut problems = Vec::new();
    for page in &pages {
        let output = page.output(&man_dir);
        process::create_dir(output.parent().expect("man page has a section directory"))?;
        let input = std::fs::File::open(&page.source)
            .context(|| format!("opening {}", page.source.display()))?;
        let out_file =
            std::fs::File::create(&output).context(|| format!("creating {}", output.display()))?;
        let mut cmd = Command::new("scdoc");
        cmd.stdin(input).stdout(out_file).stderr(Stdio::piped());
        let result = process::output(ctx, &mut cmd);
        if let Err(err) = result {
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

/// Every `ob` command and every orthros.toml key must be documented.
fn check_coverage(ctx: &Ctx) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let ob_page = read(ctx, "docs/man/ob.1.scd")?;
    for (name, _) in COMMANDS {
        if !ob_page.contains(&format!("*{name}*")) {
            problems.push(format!("docs/man/ob.1.scd does not document the `{name}` command"));
        }
    }
    let config_page = read(ctx, "docs/man/orthros.toml.5.scd")?;
    for key in config::KEYS {
        if !config_page.contains(&format!("*{key}*")) {
            problems.push(format!("docs/man/orthros.toml.5.scd does not document `{key}`"));
        }
    }
    Ok(problems)
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
        let page = Page::from_path(Path::new("docs/man/orthros.toml.5.scd")).unwrap();
        assert_eq!((page.name.as_str(), page.section.as_str()), ("orthros.toml", "5"));
        assert_eq!(page.output(Path::new("out")), Path::new("out/man5/orthros.toml.5"));
        assert!(Page::from_path(Path::new("docs/man/README.scd")).is_none());
        assert!(Page::from_path(Path::new("docs/man/ob.x.scd")).is_none());
    }
}
