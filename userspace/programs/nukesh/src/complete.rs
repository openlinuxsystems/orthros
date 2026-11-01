// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Programmable completion, as bash has it: `complete` says how a
//! command's arguments are completed (word lists, files, a shell function
//! that fills `COMPREPLY`), `compgen` runs a specification, and `compopt`
//! changes one while it runs. Scripts written for bash's completion work
//! unchanged, among them the ones the programs' argument parsers generate.
//! A command that `complete` says nothing for completes from its
//! completion file, if it has one (`files`).

mod files;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::rc::Rc;

use crate::builtins::{TABLE, flags, out, usage};
use crate::pattern::{Options, Pattern};
use crate::shell::{Flow, Res, Shell};
use crate::{sys, text, trap};

/// How one command's words are completed.
#[derive(Clone, Debug, Default)]
pub struct Spec {
    /// `-A` actions and their letters: `file`, `directory`, `command`...
    pub actions: Vec<String>,
    pub glob: Option<String>,
    pub words: Option<String>,
    pub function: Option<String>,
    pub command: Option<String>,
    pub filter: Option<String>,
    pub prefix: String,
    pub suffix: String,
    /// `-o` options: `default`, `dirnames`, `filenames`, `nospace`...
    pub options: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct Specs {
    pub commands: HashMap<String, Spec>,
    /// `-D`, `-E` and `-I`.
    pub default: Option<Spec>,
    pub empty: Option<Spec>,
    pub initial: Option<Spec>,
    /// The options `compopt` changes while a completion runs.
    pub current: Option<BTreeSet<String>>,
    /// The completion files read so far, by program, and the programs
    /// found to have none.
    pub files: HashMap<String, Option<Rc<files::CompletionFile>>>,
}

const ACTIONS: &[(char, &str)] = &[
    ('a', "alias"),
    ('b', "builtin"),
    ('c', "command"),
    ('d', "directory"),
    ('e', "export"),
    ('f', "file"),
    ('g', "group"),
    ('j', "job"),
    ('k', "keyword"),
    ('s', "service"),
    ('u', "user"),
    ('v', "variable"),
];

const ALL_ACTIONS: &[&str] = &[
    "alias",
    "arrayvar",
    "binding",
    "builtin",
    "command",
    "directory",
    "disabled",
    "enabled",
    "export",
    "file",
    "function",
    "group",
    "helptopic",
    "hostname",
    "job",
    "keyword",
    "running",
    "service",
    "setopt",
    "shopt",
    "signal",
    "stopped",
    "user",
    "variable",
];

const OPTIONS: &[&str] = &[
    "bashdefault",
    "default",
    "dirnames",
    "filenames",
    "noquote",
    "nosort",
    "nospace",
    "plusdirs",
];

/// A spec and the names it is for, parsed from `complete`'s or
/// `compgen`'s arguments.
struct Parsed {
    spec: Spec,
    names: Vec<String>,
    print: bool,
    remove: bool,
    default: bool,
    empty: bool,
    initial: bool,
}

fn parse_spec(shell: &Shell, name: &str, args: &[String]) -> Result<Parsed, i32> {
    let flags = flags(shell, name, args, "abcdefgjksuvprDEIo:A:G:W:F:C:X:P:S:", false)?;
    let mut spec = Spec::default();
    for (letter, action) in ACTIONS {
        if flags.has(*letter) {
            spec.actions.push(action.to_string());
        }
    }
    for action in flags.values('A') {
        if !ALL_ACTIONS.contains(&action) {
            shell.error(&format!("{name}: {action}: invalid action name"));
            return Err(2);
        }
        spec.actions.push(action.to_string());
    }
    for option in flags.values('o') {
        if !OPTIONS.contains(&option) {
            shell.error(&format!("{name}: {option}: invalid option name"));
            return Err(2);
        }
        spec.options.insert(option.to_string());
    }
    spec.glob = flags.value('G').map(str::to_string);
    spec.words = flags.value('W').map(str::to_string);
    spec.function = flags.value('F').map(str::to_string);
    spec.command = flags.value('C').map(str::to_string);
    spec.filter = flags.value('X').map(str::to_string);
    spec.prefix = flags.value('P').unwrap_or("").to_string();
    spec.suffix = flags.value('S').unwrap_or("").to_string();
    Ok(Parsed {
        spec,
        print: flags.has('p'),
        remove: flags.has('r'),
        default: flags.has('D'),
        empty: flags.has('E'),
        initial: flags.has('I'),
        names: flags.rest,
    })
}

/// A spec as `complete -p` prints it.
fn spec_line(spec: &Spec, target: &str) -> String {
    let mut line = String::from("complete");
    for option in &spec.options {
        line.push_str(&format!(" -o {option}"));
    }
    for action in &spec.actions {
        match ACTIONS.iter().find(|(_, a)| a == action) {
            Some((letter, _)) => line.push_str(&format!(" -{letter}")),
            None => line.push_str(&format!(" -A {action}")),
        }
    }
    let quoted = |s: &str| crate::expand::quote_always(s);
    if let Some(g) = &spec.glob {
        line.push_str(&format!(" -G {}", quoted(g)));
    }
    if let Some(w) = &spec.words {
        line.push_str(&format!(" -W {}", quoted(w)));
    }
    if let Some(f) = &spec.function {
        line.push_str(&format!(" -F {f}"));
    }
    if let Some(c) = &spec.command {
        line.push_str(&format!(" -C {}", quoted(c)));
    }
    if let Some(x) = &spec.filter {
        line.push_str(&format!(" -X {}", quoted(x)));
    }
    if !spec.prefix.is_empty() {
        line.push_str(&format!(" -P {}", quoted(&spec.prefix)));
    }
    if !spec.suffix.is_empty() {
        line.push_str(&format!(" -S {}", quoted(&spec.suffix)));
    }
    line.push(' ');
    line.push_str(target);
    line.push('\n');
    line
}

pub fn complete(shell: &mut Shell, args: &[String]) -> Res {
    let parsed = match parse_spec(shell, "complete", &args[1..]) {
        Ok(parsed) => parsed,
        Err(status) => return Ok(status),
    };
    let specials = parsed.default || parsed.empty || parsed.initial;
    let nothing_given = parsed.spec.actions.is_empty()
        && parsed.spec.options.is_empty()
        && parsed.spec.words.is_none()
        && parsed.spec.function.is_none()
        && parsed.spec.command.is_none()
        && parsed.spec.glob.is_none()
        && parsed.spec.filter.is_none();
    if parsed.print || (nothing_given && parsed.names.is_empty() && !parsed.remove && !specials) {
        let mut text = String::new();
        let specs = &shell.completions;
        if parsed.names.is_empty() {
            if let Some(spec) = &specs.default {
                text.push_str(&spec_line(spec, "-D"));
            }
            if let Some(spec) = &specs.empty {
                text.push_str(&spec_line(spec, "-E"));
            }
            if let Some(spec) = &specs.initial {
                text.push_str(&spec_line(spec, "-I"));
            }
            let mut names: Vec<&String> = specs.commands.keys().collect();
            names.sort();
            for name in names {
                text.push_str(&spec_line(&specs.commands[name], name));
            }
        } else {
            for name in &parsed.names {
                match specs.commands.get(name) {
                    Some(spec) => text.push_str(&spec_line(spec, name)),
                    None => {
                        shell.error(&format!("complete: {name}: no completion specification"));
                        out(shell, "complete", &text);
                        return Ok(1);
                    }
                }
            }
        }
        return Ok(i32::from(!out(shell, "complete", &text)));
    }
    if parsed.remove {
        if parsed.default {
            shell.completions.default = None;
        }
        if parsed.empty {
            shell.completions.empty = None;
        }
        if parsed.initial {
            shell.completions.initial = None;
        }
        if parsed.names.is_empty() && !specials {
            shell.completions.commands.clear();
        }
        for name in &parsed.names {
            shell.completions.commands.remove(name);
        }
        return Ok(0);
    }
    if parsed.default {
        shell.completions.default = Some(parsed.spec.clone());
    }
    if parsed.empty {
        shell.completions.empty = Some(parsed.spec.clone());
    }
    if parsed.initial {
        shell.completions.initial = Some(parsed.spec.clone());
    }
    if parsed.names.is_empty() && !specials {
        return usage(shell, "complete", "");
    }
    for name in parsed.names {
        shell.completions.commands.insert(name, parsed.spec.clone());
    }
    Ok(0)
}

pub fn compopt(shell: &mut Shell, args: &[String]) -> Res {
    // `-o x` turns x on, `+o x` off.
    let mut on = Vec::new();
    let mut off = Vec::new();
    let mut names = Vec::new();
    let mut targets = (false, false, false);
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-o" | "+o" => {
                let Some(option) = args.get(i + 1) else {
                    return usage(shell, "compopt", "-o: option requires an argument");
                };
                if !OPTIONS.contains(&option.as_str()) {
                    shell.error(&format!("compopt: {option}: invalid option name"));
                    return Ok(2);
                }
                if arg == "-o" {
                    on.push(option.clone());
                } else {
                    off.push(option.clone());
                }
                i += 2;
                continue;
            }
            "-D" => targets.0 = true,
            "-E" => targets.1 = true,
            "-I" => targets.2 = true,
            _ => names.push(arg.to_string()),
        }
        i += 1;
    }
    let change = |options: &mut BTreeSet<String>| {
        for o in &on {
            options.insert(o.clone());
        }
        for o in &off {
            options.remove(o);
        }
    };
    if names.is_empty() && targets == (false, false, false) {
        match &mut shell.completions.current {
            Some(current) => {
                change(current);
                return Ok(0);
            }
            None => {
                shell.error("compopt: not currently executing completion function");
                return Ok(1);
            }
        }
    }
    let specs = &mut shell.completions;
    if targets.0
        && let Some(spec) = &mut specs.default
    {
        change(&mut spec.options);
    }
    if targets.1
        && let Some(spec) = &mut specs.empty
    {
        change(&mut spec.options);
    }
    if targets.2
        && let Some(spec) = &mut specs.initial
    {
        change(&mut spec.options);
    }
    let mut status = 0;
    for name in names {
        match specs.commands.get_mut(&name) {
            Some(spec) => change(&mut spec.options),
            None => status = 1,
        }
    }
    Ok(status)
}

pub fn compgen(shell: &mut Shell, args: &[String]) -> Res {
    let parsed = match parse_spec(shell, "compgen", &args[1..]) {
        Ok(parsed) => parsed,
        Err(status) => return Ok(status),
    };
    let word = parsed.names.first().cloned().unwrap_or_default();
    let results = generate(shell, &parsed.spec, &word, None)?;
    if results.is_empty() {
        return Ok(1);
    }
    let mut text = String::new();
    for result in &results {
        let _ = writeln!(text, "{result}");
    }
    Ok(i32::from(!out(shell, "compgen", &text)))
}

/// The words of the command being completed, as the function sees them.
pub struct Context {
    pub words: Vec<String>,
    pub index: usize,
    pub line: String,
    pub point: usize,
}

/// Runs a spec for `word`: the candidates, not yet quoted.
pub fn generate(
    shell: &mut Shell,
    spec: &Spec,
    word: &str,
    context: Option<&Context>,
) -> Result<Vec<String>, Flow> {
    let mut results: Vec<String> = Vec::new();
    for action in &spec.actions {
        results.extend(action_words(shell, action, word));
    }
    if let Some(glob) = &spec.glob {
        let options = crate::glob::GlobOptions {
            extglob: shell.shopt("extglob"),
            dotglob: shell.shopt("dotglob"),
            ..Default::default()
        };
        results.extend(crate::glob::glob(glob, options));
    }
    if let Some(list) = &spec.words {
        let words = match crate::parse::words(list, shell.parse_options()) {
            Ok(words) => shell.expand_words(&words)?,
            Err(_) => Vec::new(),
        };
        results.extend(words.into_iter().filter(|w| w.starts_with(word)));
    }
    if let Some(function) = &spec.function
        && let Some(context) = context
    {
        results.extend(run_function(shell, function, word, context)?);
    }
    if let Some(command) = &spec.command
        && let Some(context) = context
    {
        let previous = context
            .index
            .checked_sub(1)
            .and_then(|i| context.words.get(i))
            .cloned()
            .unwrap_or_default();
        let cmd = context.words.first().cloned().unwrap_or_default();
        let line = format!(
            "{command} {} {} {}",
            crate::expand::quote(&cmd),
            crate::expand::quote(word),
            crate::expand::quote(&previous)
        );
        let program = crate::parse::parse(&line, "complete");
        if let Ok(program) = program {
            let output = shell.command_subst(&program)?;
            results.extend(output.lines().map(str::to_string));
        }
    }
    if let Some(filter) = &spec.filter {
        // `-X pattern` drops what matches; `!pattern` keeps only that.
        let (keep, pattern) = match filter.strip_prefix('!') {
            Some(p) => (true, p.to_string()),
            None => (false, filter.clone()),
        };
        let pattern = pattern.replace('&', &crate::pattern::escape(word));
        let matcher = Pattern::new(
            &pattern,
            Options { extglob: shell.shopt("extglob"), ..Options::default() },
        );
        results.retain(|r| matcher.matches(r) == keep);
    }
    if !spec.prefix.is_empty() || !spec.suffix.is_empty() {
        for r in &mut results {
            *r = format!("{}{r}{}", spec.prefix, spec.suffix);
        }
    }
    Ok(results)
}

/// Calls a completion function with `COMP_WORDS` and its kin set: what
/// it put in `COMPREPLY`.
fn run_function(
    shell: &mut Shell,
    function: &str,
    word: &str,
    context: &Context,
) -> Result<Vec<String>, Flow> {
    let Some(def) = shell.functions.get(function).cloned() else { return Ok(Vec::new()) };
    let words: Vec<(Option<String>, String)> =
        context.words.iter().map(|w| (None, w.clone())).collect();
    let _ = shell.assign_array("COMP_WORDS", words, false);
    shell.set_var("COMP_CWORD", &context.index.to_string());
    shell.set_var("COMP_LINE", &context.line);
    shell.set_var("COMP_POINT", &context.point.to_string());
    shell.set_var("COMP_TYPE", "9");
    shell.set_var("COMP_KEY", "9");
    shell.unset("COMPREPLY");
    let previous = context
        .index
        .checked_sub(1)
        .and_then(|i| context.words.get(i))
        .cloned()
        .unwrap_or_default();
    let cmd = context.words.first().cloned().unwrap_or_default();
    let args = vec![function.to_string(), cmd, word.to_string(), previous];
    // The function's output would scramble the line being edited.
    let saved = sys::dup_high(1).ok();
    if let Ok(null) = sys::open("/dev/null", libc::O_WRONLY, 0) {
        let _ = sys::dup2(null, 1);
        sys::close(null);
    }
    let status = shell.call_function(def, &args);
    if let Some(saved) = saved {
        let _ = sys::dup2(saved, 1);
        sys::close(saved);
    }
    let status = status?;
    let reply = shell.get_value("COMPREPLY").map_or_default(|v| v.elements());
    // 124: the function loaded a new spec; the caller tries again.
    if status == 124 {
        shell.completions_retry = true;
    }
    Ok(reply)
}

/// The words of one action that start with `word`.
fn action_words(shell: &mut Shell, action: &str, word: &str) -> Vec<String> {
    let mut out: Vec<String> = match action {
        "alias" => shell.aliases.keys().cloned().collect(),
        "builtin" => TABLE.iter().map(|b| b.name.to_string()).collect(),
        "enabled" => TABLE
            .iter()
            .filter(|b| crate::builtins::enabled(shell, b.name))
            .map(|b| b.name.to_string())
            .collect(),
        "disabled" => shell.disabled.iter().cloned().collect(),
        "keyword" => crate::builtins::KEYWORDS.iter().map(ToString::to_string).collect(),
        "function" => shell.functions.keys().cloned().collect(),
        "variable" => shell.vars.all().keys().map(ToString::to_string).collect(),
        "arrayvar" => shell
            .vars
            .all()
            .into_iter()
            .filter(|(_, v)| {
                matches!(v.value, Some(crate::vars::Value::Array(_) | crate::vars::Value::Assoc(_)))
            })
            .map(|(k, _)| k.to_string())
            .collect(),
        "export" => shell
            .vars
            .all()
            .into_iter()
            .filter(|(_, v)| v.has(crate::vars::attr::EXPORT))
            .map(|(k, _)| k.to_string())
            .collect(),
        "setopt" => crate::shell::OPTION_TABLE.iter().map(|(n, _)| n.to_string()).collect(),
        "shopt" => crate::shell::SHOPTS.iter().map(|(n, _)| n.to_string()).collect(),
        "signal" => trap::names().iter().map(|(n, _)| format!("SIG{n}")).collect(),
        "helptopic" => TABLE.iter().map(|b| b.name.to_string()).collect(),
        "user" => sys::user_names(),
        "group" => groups(),
        "hostname" => hostnames(),
        "service" => services(),
        "job" | "running" | "stopped" => shell
            .jobs
            .list
            .iter()
            .filter(|j| action == "job" || (action == "stopped") == j.stopped())
            .map(|j| j.text.split_whitespace().next().unwrap_or("").to_string())
            .collect(),
        "command" => return commands(shell, word),
        "file" => return files(shell, word, false),
        "directory" => return files(shell, word, true),
        _ => Vec::new(),
    };
    out.retain(|w| w.starts_with(word));
    out.sort();
    out.dedup();
    out
}

fn groups() -> Vec<String> {
    let Ok(data) = std::fs::read_to_string("/etc/group") else { return Vec::new() };
    data.lines()
        .filter_map(|l| l.split(':').next())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

fn hostnames() -> Vec<String> {
    let mut names = vec![sys::hostname()];
    if let Ok(data) = std::fs::read_to_string("/etc/hosts") {
        for line in data.lines() {
            let line = line.split('#').next().unwrap_or("");
            names.extend(line.split_whitespace().skip(1).map(str::to_string));
        }
    }
    names
}

/// Spark's services, as `service` takes them: the names of their files.
fn services() -> Vec<String> {
    let mut names = Vec::new();
    for dir in ["/system/settings/spark/services", "/etc/spark/services"] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = text::from_os(&entry.file_name());
                if let Some(service) = name.strip_suffix(".service") {
                    names.push(service.to_string());
                }
            }
        }
    }
    names
}

/// Commands a first word can be: aliases, functions, builtins, reserved
/// words and programs on `PATH`.
pub fn commands(shell: &mut Shell, word: &str) -> Vec<String> {
    if word.contains('/') {
        return files(shell, word, false)
            .into_iter()
            .filter(|f| f.ends_with('/') || sys::executable(&shell.tilde_expand_word(f)))
            .collect();
    }
    let loose = shell.completions_loose;
    let fits = |name: &str| if loose { tier(word, name).is_some() } else { name.starts_with(word) };
    let mut names: BTreeSet<String> = BTreeSet::new();
    names.extend(shell.aliases.keys().filter(|k| fits(k)).cloned());
    names.extend(shell.functions.keys().filter(|k| fits(k)).cloned());
    names.extend(TABLE.iter().filter(|b| fits(b.name)).map(|b| b.name.to_string()));
    names.extend(crate::builtins::KEYWORDS.iter().filter(|k| fits(k)).map(ToString::to_string));
    let path = shell.get("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let Ok(entries) = std::fs::read_dir(text::to_os(dir)) else { continue };
        for entry in entries.flatten() {
            let name = text::from_os(&entry.file_name());
            if fits(&name) && !names.contains(&name) {
                let full = format!("{dir}/{name}");
                if std::fs::metadata(text::to_os(&full)).is_ok_and(|m| m.is_file())
                    && sys::executable(&full)
                {
                    names.insert(name);
                }
            }
        }
    }
    let names: Vec<String> = names.into_iter().collect();
    if loose { best_tier(word, names) } else { names }
}

/// How loosely `name` matches `word` when nothing starts with it (ADR
/// 0273): 1 if it starts with it ignoring case, 2 if its start is one
/// typo away (two for words of seven letters or more); none for words
/// under three letters, where a typo matches too much.
pub fn tier(word: &str, name: &str) -> Option<u8> {
    if name.starts_with(word) {
        return Some(0);
    }
    let lower = word.to_lowercase();
    if name.to_lowercase().starts_with(&lower) {
        return Some(1);
    }
    let length = word.chars().count();
    if length < 3 {
        return None;
    }
    let most = if length >= 7 { 2 } else { 1 };
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let close = (length.saturating_sub(1)..=length + 1).any(|n| {
        n <= name.len()
            && crate::suggest::distance(&lower, &name[..n].iter().collect::<String>()) <= most
    });
    close.then_some(2)
}

/// Of loose matches, those of the best tier.
fn best_tier(word: &str, names: Vec<String>) -> Vec<String> {
    let base = |n: &str| n.trim_end_matches('/').rsplit('/').next().unwrap_or(n).to_string();
    let word = word.rsplit('/').next().unwrap_or(word);
    let Some(best) = names.iter().filter_map(|n| tier(word, &base(n))).min() else { return names };
    names.into_iter().filter(|n| tier(word, &base(n)) == Some(best)).collect()
}

/// `...` as `../..`, `....` as `../../..`, in each part of a path being
/// completed (ADR 0273); `None` when there is none.
pub fn expand_dots(word: &str) -> Option<String> {
    let mut changed = false;
    let parts: Vec<String> = word
        .split('/')
        .map(|part| {
            if part.len() >= 3 && part.bytes().all(|b| b == b'.') {
                changed = true;
                vec![".."; part.len() - 1].join("/")
            } else {
                part.to_string()
            }
        })
        .collect();
    changed.then(|| parts.join("/"))
}

impl Shell {
    /// `~` or `~user` at the start of a word being completed, expanded.
    pub fn tilde_expand_word(&self, word: &str) -> String {
        if let Some(rest) = word.strip_prefix('~') {
            let (user, tail) = match rest.find('/') {
                Some(slash) => (&rest[..slash], &rest[slash..]),
                None => (rest, ""),
            };
            if let Some(home) = self.tilde(user) {
                return format!("{home}{tail}");
            }
        }
        word.to_string()
    }
}

/// File names that complete `word`: directories with a `/`; with
/// `dirs_only`, only directories.
pub fn files(shell: &Shell, word: &str, dirs_only: bool) -> Vec<String> {
    if word.starts_with('~') && !word.contains('/') {
        // `~us`: user names.
        return sys::user_names()
            .into_iter()
            .filter(|u| u.starts_with(&word[1..]))
            .map(|u| format!("~{u}/"))
            .collect();
    }
    let expanded = shell.tilde_expand_word(word);
    let (dir, base, shown_dir) = match expanded.rfind('/') {
        Some(slash) => (
            if slash == 0 { "/".to_string() } else { expanded[..slash].to_string() },
            expanded[slash + 1..].to_string(),
            word[..word.rfind('/').map_or(0, |s| s + 1)].to_string(),
        ),
        None => (".".to_string(), expanded.clone(), String::new()),
    };
    let Ok(entries) = std::fs::read_dir(text::to_os(&dir)) else { return Vec::new() };
    let hidden = hidden_names(&dir);
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = text::from_os(&entry.file_name());
        let fits = if shell.completions_loose {
            tier(&base, &name).is_some()
        } else {
            name.starts_with(&base)
        };
        if !fits {
            continue;
        }
        if name.starts_with('.') && !base.starts_with('.') && !shell.shopt("dotglob") {
            continue;
        }
        if name != base && hidden.contains(&name) && !shell.settings.show_hidden {
            continue;
        }
        let full = format!("{}/{name}", dir.trim_end_matches('/'));
        let is_dir = std::fs::metadata(text::to_os(&full)).is_ok_and(|m| m.is_dir());
        if dirs_only && !is_dir {
            continue;
        }
        out.push(format!("{shown_dir}{name}{}", if is_dir { "/" } else { "" }));
    }
    out.sort();
    if shell.completions_loose { best_tier(&base, out) } else { out }
}

/// The names `dir/.hidden` lists, one a line, which ls(1) and the file
/// manager leave out, as GNOME Files does: on Dukovo, the Unix names of
/// the root's folders, links to the plainly named ones (ADR 0113). They
/// complete only once the word is the whole name.
fn hidden_names(dir: &str) -> Vec<String> {
    let path = format!("{}/.hidden", dir.trim_end_matches('/'));
    std::fs::read(text::to_os(&path)).map_or_else(
        |_| Vec::new(),
        |bytes| text::decode(&bytes).lines().filter(|l| !l.is_empty()).map(String::from).collect(),
    )
}

// The completion the line editor asks for.

/// One way to finish the word at the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// What replaces the word.
    pub text: String,
    /// What the menu shows.
    pub display: String,
    /// Whether a space goes after it when it is the only one.
    pub space: bool,
    /// What it does, as the menu shows beside it: an option's.
    pub description: Option<String>,
}

/// The words of a command line up to the cursor: each word and where it
/// starts, for the command the cursor is in.
fn split_line(line: &str) -> (Vec<(usize, String)>, bool) {
    let mut words: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut start = None;
    let mut quote: Option<char> = None;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some((_, next)) = chars.next() {
                    current.push(next);
                }
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                start.get_or_insert(at);
                quote = Some(c);
            }
            '\\' => {
                start.get_or_insert(at);
                if let Some((_, next)) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' | '\n' => {
                if let Some(s) = start.take() {
                    words.push((s, std::mem::take(&mut current)));
                }
            }
            // A new command starts after these.
            ';' | '&' | '|' | '(' | ')' => {
                if let Some(s) = start.take() {
                    words.push((s, std::mem::take(&mut current)));
                }
                words.clear();
            }
            _ => {
                start.get_or_insert(at);
                current.push(c);
            }
        }
    }
    let open = start.is_some();
    if let Some(s) = start {
        words.push((s, current));
    }
    (words, open || quote.is_some())
}

/// Backslashes before the characters the shell would take as syntax.
fn quote_completion(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if " \t\n\\'\"`$&|;()<>!{}*?[]#~=%".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Shell {
    /// Completes the word before byte `pos` of `line`: where the word
    /// starts, and the candidates.
    /// Tab: the completions of the word before `pos`. `...` in it means
    /// `../..`; when nothing starts with it, file and command names that
    /// match ignoring case or with a typo are offered (ADR 0273).
    pub fn complete_line(&mut self, line: &str, pos: usize) -> (usize, Vec<Candidate>) {
        let pos = pos.min(line.len());
        let (words, in_word) = split_line(&line[..pos]);
        if in_word
            && let Some((start, word)) = words.last().cloned()
            && let Some(expanded) = expand_dots(&word)
        {
            if word.ends_with("...") || word.ends_with(".../") {
                let text = format!("{}/", expanded.trim_end_matches('/'));
                return (
                    start,
                    vec![Candidate {
                        display: text.clone(),
                        text,
                        space: false,
                        description: None,
                    }],
                );
            }
            let changed = format!("{}{expanded}{}", &line[..start], &line[pos..]);
            return self.complete_line(&changed, start + expanded.len());
        }
        let found = self.complete_exactly(line, pos);
        if !found.1.is_empty() || !in_word {
            return found;
        }
        self.completions_loose = true;
        let loose = self.complete_exactly(line, pos);
        self.completions_loose = false;
        loose
    }

    fn complete_exactly(&mut self, line: &str, pos: usize) -> (usize, Vec<Candidate>) {
        let before = &line[..pos];
        let (mut words, in_word) = split_line(before);
        if !in_word {
            words.push((pos, String::new()));
        }
        let index = words.len() - 1;
        let (start, word) = words[index].clone();
        let word_is_command = index == 0
            || (index > 0
                && matches!(
                    words[index - 1].1.as_str(),
                    "sudo" | "elevate" | "command" | "exec" | "nohup" | "time" | "env"
                ));
        // A variable name after `$`.
        if let Some(name) = word.strip_prefix('$')
            && !name.contains('/')
        {
            let braced = name.starts_with('{');
            let name = name.trim_start_matches('{');
            let mut names: Vec<String> = self
                .vars
                .all()
                .keys()
                .filter(|k| k.starts_with(name))
                .map(ToString::to_string)
                .collect();
            names.sort();
            let candidates = names
                .into_iter()
                .map(|n| Candidate {
                    text: if braced { format!("${{{n}}}") } else { format!("${n}") },
                    display: n,
                    space: false,
                    description: None,
                })
                .collect();
            return (start, candidates);
        }
        let context = Context {
            words: words.iter().map(|(_, w)| w.clone()).collect(),
            index,
            line: line.to_string(),
            point: pos,
        };
        let mut results: Option<(Vec<String>, BTreeSet<String>)> = None;
        if !word_is_command || index > 0 && !matches!(words[0].1.as_str(), "sudo" | "elevate") {
            let command = words[0].1.clone();
            for _attempt in 0..2 {
                let base = command.rsplit('/').next().unwrap_or(&command).to_string();
                let spec = self
                    .completions
                    .commands
                    .get(&command)
                    .or_else(|| self.completions.commands.get(&base))
                    .or(self.completions.default.as_ref())
                    .cloned();
                let Some(spec) = spec else { break };
                self.completions.current = Some(spec.options.clone());
                self.completions_retry = false;
                let generated = self.generate_quietly(&spec, &word, &context);
                let options = self.completions.current.take().unwrap_or_default();
                if self.completions_retry {
                    continue;
                }
                results = Some((generated, options));
                break;
            }
        }
        if results.is_none() && !word_is_command {
            let program = words[0].1.rsplit('/').next().unwrap_or_default().to_string();
            if let Some(file) = self.completion_file(&program) {
                let all: Vec<String> = words.iter().map(|(_, w)| w.clone()).collect();
                let candidates = match self.complete_from_file(&file, &all, index) {
                    files::FromFile::Options(candidates) => candidates,
                    files::FromFile::Words { list, is_files, prefix, word } => {
                        let mut candidates =
                            Self::candidates(&word, list, &BTreeSet::new(), is_files, false);
                        for candidate in &mut candidates {
                            candidate.text.insert_str(0, &prefix);
                        }
                        candidates
                    }
                };
                return (start, candidates);
            }
        }
        let (list, options, is_files) = match results {
            Some((list, options))
                if !list.is_empty()
                    || !(options.contains("default")
                        || options.contains("bashdefault")
                        || options.contains("dirnames")) =>
            {
                let is_files = options.contains("filenames");
                let mut list = list;
                if options.contains("plusdirs") {
                    list.extend(files(self, &word, true));
                }
                (list, options, is_files)
            }
            Some((_, options)) if options.contains("dirnames") => {
                (files(self, &word, true), options, true)
            }
            Some((_, options)) => {
                let list = if word_is_command && index == 0 {
                    commands(self, &word)
                } else {
                    files(self, &word, false)
                };
                (list, options, true)
            }
            None if word_is_command => (commands(self, &word), BTreeSet::new(), word.contains('/')),
            None => (files(self, &word, false), BTreeSet::new(), true),
        };
        (start, Self::candidates(&word, list, &options, is_files, word_is_command))
    }

    /// The candidates for `word` from `list`, as `options` (`compopt`'s)
    /// say: sorted, quoted, and shown by their last part if they are
    /// files.
    fn candidates(
        word: &str,
        mut list: Vec<String>,
        options: &BTreeSet<String>,
        is_files: bool,
        word_is_command: bool,
    ) -> Vec<Candidate> {
        if !options.contains("nosort") {
            list.sort();
        }
        list.dedup();
        let quote = !options.contains("noquote") && (is_files || word_is_command);
        list.into_iter()
            .map(|item| {
                let dir = is_files && item.ends_with('/');
                let display = if is_files {
                    let trimmed = item.trim_end_matches('/');
                    let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
                    format!("{base}{}", if dir { "/" } else { "" })
                } else {
                    item.clone()
                };
                let text = if quote {
                    // A leading `~` stays as it is.
                    match item.strip_prefix('~') {
                        Some(rest) if word.starts_with('~') => {
                            let (user, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                            format!("~{user}{}", quote_completion(tail))
                        }
                        _ => quote_completion(&item),
                    }
                } else {
                    item
                };
                Candidate {
                    text,
                    display,
                    space: !dir && !options.contains("nospace"),
                    description: None,
                }
            })
            .collect()
    }

    /// Runs a spec while the user edits a line: errors and Ctrl-C are not
    /// the line's business.
    fn generate_quietly(&mut self, spec: &Spec, word: &str, context: &Context) -> Vec<String> {
        let saved_status = self.status;
        let saved_errexit = self.opts.errexit;
        self.opts.errexit = false;
        let result = generate(self, spec, word, Some(context));
        self.opts.errexit = saved_errexit;
        self.status = saved_status;
        result.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_forgives_case_and_typos() {
        assert_eq!(tier("Doc", "Documents"), Some(0));
        assert_eq!(tier("doc", "Documents"), Some(1));
        assert_eq!(tier("Dcou", "Documents"), Some(2));
        assert_eq!(tier("ehco", "echo"), Some(2));
        assert_eq!(tier("dw", "Downloads"), None);
        assert_eq!(tier("xyz", "Documents"), None);
        let names =
            vec!["Desktop/".to_string(), "documents/".to_string(), "Dcouments/".to_string()];
        assert_eq!(best_tier("Docu", names), ["documents/"]);
    }

    #[test]
    fn dots_climb_folders() {
        assert_eq!(expand_dots("...").as_deref(), Some("../.."));
        assert_eq!(expand_dots("..../src").as_deref(), Some("../../../src"));
        assert_eq!(expand_dots("../a"), None);
        assert_eq!(expand_dots("a...b"), None);
    }

    #[test]
    fn lines_split_at_the_cursor() {
        let (words, open) = split_line("ls -l fo");
        assert!(open);
        assert_eq!(words.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>(), ["ls", "-l", "fo"]);
        let (words, open) = split_line("echo a | gr");
        assert!(open);
        assert_eq!(words.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>(), ["gr"]);
        let (words, open) = split_line("cat 'my fi");
        assert!(open);
        assert_eq!(words[1].1, "my fi");
    }

    #[test]
    fn word_lists_complete() {
        let mut shell = Shell::new("test", false);
        let spec = Spec { words: Some("start stop restart".into()), ..Spec::default() };
        assert_eq!(generate(&mut shell, &spec, "st", None).unwrap(), ["start", "stop"]);
    }
}
