// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The manual pages of the boot archive, and the shell's completions,
//! which come from them (ADR 0102).
//!
//! Dukovo's own pages (docs/man) join the ports'. makewhatis(8) indexes
//! them all, so that man(1), apropos(1) and whatis(1) find a page by any
//! name its NAME section gives, as *man poweroff* finds reboot(8). Every
//! program in `/system/bin` must have a page, by its file's name or by
//! one of those names, or the build fails. And from each program's page
//! come the options the shell completes for it: the build reads the
//! page as mandoc(1) shows it, takes each option a list of them names,
//! with what its argument is and the first sentence of what it does,
//! and writes them to the program's completion file (nukesh(1),
//! COMPLETION FILES).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::process::{self, step};
use crate::{Context, Ctx, Error, Result, docs};

/// Where the manual pages go in the boot archive.
pub const MAN_DIR: &str = "system/share/man";
/// Where the shell's completion files go in the boot archive.
pub const COMPLETIONS_DIR: &str = "system/share/nukesh/completions";
/// The programs' directory in the boot archive.
const BIN_DIR: &str = "system/bin";

/// The first two numbers of a mandoc.db(5) file: its magic number, and
/// the version of the format, which the image's mandoc (1.14.6) reads.
const DB_MAGIC: u32 = 0x3a7d_0cdb;
const DB_VERSION: u32 = 1;

/// The sections of a page that hold no list of its options.
const NOT_OPTIONS: &[&str] = &[
    "NAME",
    "SYNOPSIS",
    "EXAMPLES",
    "EXAMPLE",
    "SEE ALSO",
    "ENVIRONMENT",
    "FILES",
    "EXIT STATUS",
    "DIAGNOSTICS",
    "HISTORY",
    "AUTHORS",
    "AUTHOR",
    "BUGS",
    "REPORTING BUGS",
    "STANDARDS",
    "CAVEATS",
    "COPYRIGHT",
];

/// What the operands of the programs whose operands are not files
/// complete as: the program, the operand (from 1, or 0 for every
/// operand not named), and a completion action (`complete -A`) or the
/// words, after `words `.
const OPERANDS: &[(&str, usize, &str)] = &[
    ("7zz", 1, "words a b d e h i l rn t u x"),
    ("7z", 1, "words a b d e h i l rn t u x"),
    ("apropos", 0, "none"),
    ("chgrp", 1, "group"),
    ("chown", 1, "user"),
    ("ionice", 1, "command"),
    ("man", 0, "command"),
    ("nice", 1, "command"),
    ("nohup", 1, "command"),
    ("rtrace", 1, "command"),
    ("service", 1, "service"),
    ("service", 2, "words start stop restart status --full-restart"),
    ("stdbuf", 1, "command"),
    ("timeout", 2, "command"),
    ("watch", 1, "command"),
    ("whatis", 0, "command"),
    ("which", 0, "command"),
    ("xargs", 1, "command"),
];

/// Adds Dukovo's own manual pages, the index of every page and each
/// program's completion file to `files`, the boot archive's paths and
/// files, which hold the ports' pages and every program by now.
pub fn install(ctx: &Ctx, files: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let own = docs::image_pages(ctx)?;
    let ours: BTreeSet<String> = own.iter().map(|(path, _)| path.clone()).collect();
    if let Some((path, _)) = files.iter().find(|(path, _)| ours.contains(path)) {
        crate::bail!("a port and docs/man both give {path}");
    }
    files.extend(own);
    let prefix = format!("{MAN_DIR}/");
    let stage = ctx.out.join("manuals");
    if stage.exists() {
        std::fs::remove_dir_all(&stage).context(|| format!("removing {}", stage.display()))?;
    }
    let mut pages = Vec::new();
    for (path, file) in files.iter() {
        let Some(page) = path.strip_prefix(&prefix) else { continue };
        let staged = stage.join(page);
        process::create_dir(staged.parent().expect("a page is in a section"))?;
        match std::fs::read_link(file) {
            Ok(target) => std::os::unix::fs::symlink(&target, &staged)
                .context(|| format!("linking {}", staged.display()))?,
            Err(_) => process::copy(file, &staged)?,
        }
        pages.push(page.to_string());
    }
    pages.sort();
    step(&format!("Indexing {} manual pages, {} of them Dukovo's own", pages.len(), ours.len()));
    index(ctx, &stage)?;
    let db = ctx.out.join("sysroot").join(MAN_DIR).join("mandoc.db");
    process::copy(&stage.join("mandoc.db"), &db)?;
    files.push((format!("{MAN_DIR}/mandoc.db"), db));

    // Which page is each program's.
    let mut by_name: BTreeMap<String, String> = BTreeMap::new();
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for page in &pages {
        let Some(name) = page_name(page) else { continue };
        if !matches!(page_section(page), Some('1' | '8')) {
            continue;
        }
        let text = render(ctx, &stage, page)?;
        for alias in names(&text) {
            by_name.entry(alias).or_insert_with(|| page.clone());
        }
        // A page's file name is a name too, as the index has it.
        by_name.insert(name.to_string(), page.clone());
        texts.insert(page.clone(), text);
    }
    let bin_prefix = format!("{BIN_DIR}/");
    let mut programs: Vec<String> = files
        .iter()
        .filter_map(|(path, _)| path.strip_prefix(&bin_prefix))
        .filter(|name| !name.contains('/'))
        .map(str::to_string)
        .collect();
    programs.sort();
    programs.dedup();
    let missing: Vec<&str> =
        programs.iter().filter(|p| !by_name.contains_key(*p)).map(String::as_str).collect();
    if !missing.is_empty() {
        return Err(Error::new(format!(
            "{} programs in /{BIN_DIR} have no manual page: {}",
            missing.len(),
            missing.join(", ")
        ))
        .hint("write one in docs/man, or install the port's, or name the program in a page's NAME section"));
    }

    let dir = ctx.out.join("sysroot").join(COMPLETIONS_DIR);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).context(|| format!("removing {}", dir.display()))?;
    }
    process::create_dir(&dir)?;
    let mut options = 0;
    for program in &programs {
        let page = &by_name[program];
        let found = parse_options(&texts[page]);
        options += found.len();
        let file = dir.join(program);
        let text = completion_file(program, page, &found);
        std::fs::write(&file, text).context(|| format!("writing {}", file.display()))?;
        files.push((format!("{COMPLETIONS_DIR}/{program}"), file));
    }
    step(&format!(
        "Every one of the {} programs has its manual page, and its completion file ({options} options)",
        programs.len()
    ));
    Ok(())
}

/// Runs makewhatis(8) on the pages in `stage` and checks that the index
/// is one the image's mandoc reads.
fn index(ctx: &Ctx, stage: &Path) -> Result<()> {
    // Debian and Ubuntu put it in /usr/sbin, which some users' PATH
    // leaves out.
    let program = process::find_program("makewhatis")
        .or_else(|| Some(PathBuf::from("/usr/sbin/makewhatis")).filter(|p| p.exists()))
        .unwrap_or_else(|| PathBuf::from("makewhatis"));
    let mut cmd = Command::new(program);
    cmd.arg(stage);
    process::output(ctx, &mut cmd)?;
    let db = stage.join("mandoc.db");
    let bytes = std::fs::read(&db).context(|| format!("reading {}", db.display()))?;
    let word = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    if word(0) != Some(DB_MAGIC) || word(4) != Some(DB_VERSION) {
        return Err(Error::new(format!(
            "the build host's makewhatis wrote an index of another format ({:x?}), which the \
             image's mandoc cannot read",
            bytes.get(..8)
        ))
        .hint("install mandoc 1.14 (`./ob doctor`)"));
    }
    Ok(())
}

/// `man1/grep.1` to `grep`.
fn page_name(page: &str) -> Option<&str> {
    let file = page.rsplit('/').next()?;
    file.rsplit_once('.').map(|(name, _)| name)
}

fn page_section(page: &str) -> Option<char> {
    page.split('/').next()?.strip_prefix("man")?.chars().next()
}

/// `page` as mandoc(1) shows it, in plain text, from the top of the
/// pages, where `.so` requests find what they name.
fn render(ctx: &Ctx, stage: &Path, page: &str) -> Result<String> {
    let mut cmd = Command::new("mandoc");
    cmd.args(["-T", "ascii", "-O", "width=78"]).arg(page).current_dir(stage);
    let out = process::output(ctx, &mut cmd)?;
    Ok(plain(&String::from_utf8_lossy(&out.stdout)))
}

/// Text without the backspaces of bold and underlined letters.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\u{8}' {
            out.pop();
        } else {
            out.push(c);
        }
    }
    out
}

/// The names a page's NAME section gives, as in "grep, egrep - ...".
fn names(text: &str) -> Vec<String> {
    let mut lines = text.lines().skip_while(|line| line.trim_end() != "NAME").skip(1);
    let Some(line) = lines.find(|line| !line.trim().is_empty()) else { return Vec::new() };
    let list = line.split(" - ").next().unwrap_or_default();
    list.split(',').map(|name| name.trim().to_string()).filter(|name| !name.is_empty()).collect()
}

/// One option that a page lists.
#[derive(Debug, PartialEq, Eq)]
struct Opt {
    /// Its forms: `-a`, `--all`.
    forms: Vec<String>,
    /// What its argument is (a completion action, or `value` for any
    /// other), if it takes one.
    argument: Option<&'static str>,
    /// Whether the argument goes right after the option, as in 7-Zip's
    /// `-o{directory}`, and not as the next word.
    glued: bool,
    /// The first sentence of what it does.
    description: String,
}

/// The options a page lists: each line of a list that starts with an
/// option at the page's left margin (as mandoc lays out `.TP`, `.It`
/// and scdoc's lists), outside the sections that hold none.
fn parse_options(text: &str) -> Vec<Opt> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found: Vec<Opt> = Vec::new();
    let mut section = "";
    // Whether the line before was a heading or a list item's tag, so
    // that this one may be a tag at the same indent.
    let mut after_tag = false;
    for (n, line) in lines.iter().enumerate() {
        if line.starts_with(|c: char| !c.is_whitespace()) {
            section = line.trim();
            // The first line below a heading may be a tag.
            after_tag = true;
            continue;
        }
        let rest = line.trim_start();
        let indent = line.len() - rest.len();
        if (1..=3).contains(&indent) {
            // A subsection's heading.
            after_tag = true;
            continue;
        }
        let previous = n.checked_sub(1).map(|p| lines[p]).unwrap_or_default();
        let previous_indent = previous.len() - previous.trim_start().len();
        // A tag follows a blank line, the text of the item before it
        // (indented further), or another item's tag at its indent; a line
        // of a paragraph that wraps to start with an option does not.
        let starts_item = previous.trim().is_empty() || previous_indent > indent || after_tag;
        after_tag = false;
        if NOT_OPTIONS.contains(&section) || rest.is_empty() || indent > 8 || !starts_item {
            continue;
        }
        let Some((mut opt, tail)) = parse_tag(rest) else { continue };
        after_tag = true;
        // The item's text: after its tag, and on the lines below it,
        // indented further, which may start after a blank line.
        let mut description = tail.trim().to_string();
        if description.starts_with('[') {
            description.clear();
        }
        let mut below = lines[n + 1..].iter().peekable();
        if description.is_empty() {
            below.next_if(|l| l.trim().is_empty());
        }
        for line in below.take(4) {
            if line.trim().is_empty() || line.len() - line.trim_start().len() <= indent {
                break;
            }
            // A word broken at a hyphen at the end of the line joins up.
            if !description.is_empty() && !description.ends_with('-') {
                description.push(' ');
            }
            description.push_str(line.trim());
        }
        opt.description = summary(&description);
        if !found.iter().any(|known| known.forms == opt.forms) {
            found.push(opt);
        }
    }
    found
}

/// The option forms at the start of a list item's tag, such as
/// `-e PATTERNS, --regexp=PATTERNS`, and what follows them.
fn parse_tag(tag: &str) -> Option<(Opt, &str)> {
    let mut opt =
        Opt { forms: Vec::new(), argument: None, glued: false, description: String::new() };
    let mut rest = tag;
    loop {
        let (form, after) = option_form(rest)?;
        opt.forms.push(form.to_string());
        rest = after;
        // An argument joined to it: `=NUM`, `[=WHEN]`, `{directory}`.
        if let Some(after) = rest.strip_prefix("[=") {
            let end = after.find(']')?;
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('=') {
            let end = after.find([' ', ',']).unwrap_or(after.len());
            opt.argument = Some(kind(&after[..end]));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('{') {
            let end = after.find('}')?;
            opt.argument = Some(kind(&after[..end]));
            opt.glued = true;
            rest = &after[end + 1..];
        }
        // Or one after a space, as in `-f FILE` or `-T <COLS>`, ended by
        // the end of the tag, another form, or the gap before the text.
        if let Some(after) = rest.strip_prefix(' ')
            && !after.starts_with(' ')
        {
            let end = after.find(' ').unwrap_or(after.len());
            let word = after[..end].trim_end_matches(',');
            let next = &after[word.len()..];
            if is_metavar(word)
                && (next.is_empty() || next.starts_with("  ") || separator(next).is_some())
            {
                opt.argument = Some(kind(word));
                rest = next;
            }
        }
        match separator(rest) {
            Some(after) => rest = after,
            None => break,
        }
    }
    // A tag ends at the end of the line or before the text, after a space.
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with(':')) {
        return None;
    }
    // less(1) writes `-kfilename, --lesskey-file=filename`: the short
    // form is `-k`, with its argument joined to it.
    if opt.argument.is_some() && opt.forms.iter().any(|f| f.starts_with("--")) {
        for form in &mut opt.forms {
            if !form.starts_with("--") && form.len() > 2 {
                form.truncate(2);
            }
        }
    }
    Some((opt, rest.trim_start_matches(':')))
}

/// One option form at the start of `text`, such as `-a`, `--all` or
/// `+cmd`, and the rest.
fn option_form(text: &str) -> Option<(&str, &str)> {
    let body = text.strip_prefix("--").or_else(|| text.strip_prefix(['-', '+']))?;
    let first = body.chars().next()?;
    if !(first.is_ascii_alphanumeric() || "?#@!".contains(first)) {
        return None;
    }
    let len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || "_.+-?#@".contains(c)))
        .unwrap_or(body.len());
    let end = text.len() - body.len() + len;
    Some((&text[..end], &text[end..]))
}

/// What comes between two forms of one option: `, `, ` | `, ` / ` or
/// ` or `, before the next form.
fn separator(text: &str) -> Option<&str> {
    [", ", " | ", " / ", " or "].iter().find_map(|sep| {
        let after = text.strip_prefix(sep)?;
        option_form(after).map(|_| after)
    })
}

/// Whether `word` names an option's argument: `FILE`, `file`, `<COLS>`,
/// `[N]`, `script-file`.
fn is_metavar(word: &str) -> bool {
    let inner = word.trim_start_matches(['<', '[']).trim_end_matches(['>', ']']);
    !inner.is_empty()
        && inner.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '.')
        && inner.chars().all(|c| c.is_ascii_alphanumeric() || "_-:.|".contains(c))
}

/// The completion action for an argument named `metavar`.
fn kind(metavar: &str) -> &'static str {
    let name = metavar.to_ascii_lowercase();
    if name.contains("dir") {
        "directory"
    } else if ["file", "path", "archive", "device"].iter().any(|k| name.contains(k)) {
        "file"
    } else if name.contains("user") || name.contains("owner") {
        "user"
    } else if name.contains("group") {
        "group"
    } else if name.contains("signal") {
        "signal"
    } else {
        "value"
    }
}

/// The longest description a completion file keeps.
const SUMMARY: usize = 72;

/// The first sentence of `text`, without its full stop, and cut at a
/// space with `...` if it is longer than [`SUMMARY`].
fn summary(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut end = text.len();
    for (i, &b) in bytes.iter().enumerate() {
        let follows_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b')');
        if matches!(b, b'.' | b';' | b':')
            && follows_word
            && bytes.get(i + 1).is_none_or(|c| c.is_ascii_whitespace())
        {
            end = i;
            break;
        }
    }
    let sentence = text[..end].trim();
    if sentence.len() <= SUMMARY {
        return sentence.to_string();
    }
    let mut cut = SUMMARY - 3;
    while !sentence.is_char_boundary(cut) {
        cut -= 1;
    }
    let cut = sentence[..cut].rfind(' ').unwrap_or(cut);
    format!("{}...", sentence[..cut].trim_end_matches([',', ' ']))
}

/// A program's completion file (nukesh(1), COMPLETION FILES).
fn completion_file(program: &str, page: &str, options: &[Opt]) -> String {
    let section = page_section(page).unwrap_or('1');
    let name = page_name(page).unwrap_or(program);
    let mut text = format!(
        "# What nukesh(1) completes for {program}, from {name}({section}): its options, and\n\
         # what its operands are. Written by ob(1) when it built the system.\n"
    );
    for opt in options {
        let argument = match (opt.argument, opt.glued) {
            (None, _) => String::new(),
            (Some(kind), false) => kind.to_string(),
            (Some(kind), true) => format!("+{kind}"),
        };
        let description = opt.description.replace('\t', " ");
        let _ = writeln!(text, "option\t{}\t{argument}\t{description}", opt.forms.join(" "));
    }
    for &(_, operand, what) in OPERANDS.iter().filter(|(p, _, _)| *p == program) {
        let _ = writeln!(text, "operand\t{operand}\t{what}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forms(opts: &[Opt]) -> Vec<String> {
        opts.iter().map(|o| o.forms.join(" ")).collect()
    }

    #[test]
    fn reads_gnu_and_clap_pages() {
        let text = "\
NAME
       grep - print lines that match patterns

DESCRIPTION
       grep searches for PATTERNS in each FILE.  Some options, such as
       --help, are general.

OPTIONS
   Generic Program Information
       --help Output a usage message and exit.

       -V, --version
              Output the version number of grep and exit.

       -e PATTERNS, --regexp=PATTERNS
              Use PATTERNS as the patterns.  This option can be used
              multiple times.

       -f FILE, --file=FILE
              Obtain patterns from FILE, one per line.

       -T, --tabsize <COLS>
              Assume tab stops at each COLS instead of 8

       --hyperlink[=<WHEN>] [default: never]
              Hyperlink file names.

EXAMPLES
       -x     Not an option of this page's list.
";
        let opts = parse_options(text);
        assert_eq!(
            forms(&opts),
            ["--help", "-V --version", "-e --regexp", "-f --file", "-T --tabsize", "--hyperlink"]
        );
        assert_eq!(opts[0].description, "Output a usage message and exit");
        assert_eq!(opts[2].argument, Some("value"));
        assert_eq!(opts[2].description, "Use PATTERNS as the patterns");
        assert_eq!(opts[1].description, "Output the version number of grep and exit");
        assert_eq!(opts[3].argument, Some("file"));
        assert_eq!(opts[5].argument, None);
        assert_eq!(opts[5].description, "Hyperlink file names");
        assert_eq!(names(text), ["grep"]);
    }

    #[test]
    fn reads_bsd_and_scdoc_pages() {
        let text = "\
NAME
     man, apropos - display manual pages

DESCRIPTION
     The options are as follows:

     -a      Display all matching manual pages.
     -C file
             Use the specified file instead of the default configuration
             file.
     -c      Copy the manual page to the standard output instead of using
             less(1) to paginate it.  This is done by default if the
             -w option is given.

SWITCHES
       -o{directory}
           Extract into directory, made if it does not exist.

       -p, -q, --pid, --quick-pid list
           Only these processes.

       -a or --search-skip-screen
           Searches start at the top.
";
        let opts = parse_options(text);
        assert_eq!(
            forms(&opts),
            ["-a", "-C", "-c", "-o", "-p -q --pid --quick-pid", "-a --search-skip-screen"]
        );
        assert_eq!(opts[1].argument, Some("file"));
        assert_eq!(
            opts[1].description,
            "Use the specified file instead of the default configuration file"
        );
        assert_eq!(
            opts[2].description,
            "Copy the manual page to the standard output instead of using less(1)..."
        );
        assert_eq!(opts[3].argument, Some("directory"));
        assert!(opts[3].glued);
        assert_eq!(opts[4].argument, Some("value"));
        assert_eq!(names(text), ["man", "apropos"]);
    }

    #[test]
    fn writes_completion_files() {
        let opts = parse_options(
            "OPTIONS\n       -C, --directory=DIR\n              Change to DIR first.\n",
        );
        let text = completion_file("chown", "man1/chown.1", &opts);
        assert!(text.contains("option\t-C --directory\tdirectory\tChange to DIR first\n"));
        assert!(text.ends_with("operand\t1\tuser\n"));
        assert_eq!(page_name("man8/reboot.8"), Some("reboot"));
        assert_eq!(page_section("man8/reboot.8"), Some('8'));
        assert_eq!(plain("b\u{8}bo\u{8}old _\u{8}u"), "bold u");
    }
}
