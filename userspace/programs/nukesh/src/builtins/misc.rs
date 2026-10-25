// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The other builtins: aliases, `type` and `hash`, `trap`, `getopts`,
//! `ulimit`, `umask`, `times`, `caller`, `enable` and `help`.

use std::fmt::Write;

use super::{TABLE, flags, is_special, lookup, out, usage};
use crate::shell::{Res, Shell};
use crate::{show, sys, trap};

/// `command -p`'s `PATH`: where the standard programs are.
pub const DEFAULT_PATH: &str = "/system/bin:/bin:/usr/bin";

/// The reserved words.
pub const KEYWORDS: &[&str] = &[
    "!", "[[", "]]", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "time", "until", "while", "{", "}",
];

pub fn alias(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "alias", &args[1..], "p", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    // POSIX lists `name=value`; bash puts `alias ` first.
    let prefix = if shell.posix() && !flags.has('p') { "" } else { "alias " };
    if flags.rest.is_empty() {
        let mut names: Vec<&String> = shell.aliases.keys().collect();
        names.sort();
        let mut text = String::new();
        for name in names {
            let _ = writeln!(text, "{prefix}{name}={}", quote_alias(&shell.aliases[name]));
        }
        return Ok(i32::from(!out(shell, "alias", &text)));
    }
    let mut status = 0;
    let mut text = String::new();
    for arg in &flags.rest {
        match arg.split_once('=') {
            Some((name, value)) => {
                if name.is_empty() || name.contains(['/', '$', '`', '=', ' ', '\t', '\n']) {
                    shell.error(&format!("alias: `{name}': invalid alias name"));
                    status = 1;
                    continue;
                }
                shell.aliases.insert(name.to_string(), value.to_string());
            }
            None => match shell.aliases.get(arg.as_str()) {
                Some(value) => text.push_str(&format!("{prefix}{arg}={}\n", quote_alias(value))),
                None => {
                    shell.error(&format!("alias: {arg}: not found"));
                    status = 1;
                }
            },
        }
    }
    Ok(if out(shell, "alias", &text) { status } else { 1 })
}

/// An alias's value, as `alias` prints it: always in single quotes.
fn quote_alias(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn unalias(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "unalias", &args[1..], "a", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('a') {
        shell.aliases.clear();
        return Ok(0);
    }
    if flags.rest.is_empty() {
        return usage(shell, "unalias", "");
    }
    let mut status = 0;
    for name in &flags.rest {
        if shell.aliases.shift_remove(name.as_str()).is_none() {
            shell.error(&format!("unalias: {name}: not found"));
            status = 1;
        }
    }
    Ok(status)
}

/// How `describe` says what a name is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Describe {
    /// `command -v`: the alias definition, the name, or the path.
    Short,
    /// `command -V` and `type`: a sentence.
    Verbose,
    /// `type -t`: one word.
    Word,
    /// `type -p`: the path, for a program.
    Path,
}

/// Says what `name` runs, as `type` and `command -v` do: whether it was
/// found. `path_only` (for `type -P` and `command -p`) looks on `PATH`
/// alone.
pub fn describe(shell: &mut Shell, name: &str, how: Describe, path_only: bool) -> bool {
    describe_all(shell, name, how, path_only, false, false)
}

fn describe_all(
    shell: &mut Shell,
    name: &str,
    how: Describe,
    path_only: bool,
    all: bool,
    no_functions: bool,
) -> bool {
    let mut text = String::new();
    let mut found = false;
    let say = |text: &mut String, short: String, verbose: String, word: &str| {
        let line = match how {
            Describe::Short => short,
            Describe::Verbose => verbose,
            Describe::Word => word.to_string(),
            Describe::Path => return,
        };
        text.push_str(&line);
        text.push('\n');
    };
    if !path_only {
        // Aliases count where they are expanded, as in bash.
        if shell.expands_aliases()
            && let Some(value) = shell.aliases.get(name)
        {
            say(
                &mut text,
                format!("alias {name}={}", quote_alias(value)),
                format!("{name} is aliased to `{value}'"),
                "alias",
            );
            found = true;
        }
        if (all || !found) && KEYWORDS.contains(&name) {
            say(&mut text, name.to_string(), format!("{name} is a shell keyword"), "keyword");
            found = true;
        }
        if (all || !found)
            && !no_functions
            && let Some(def) = shell.functions.get(name).cloned()
        {
            say(
                &mut text,
                name.to_string(),
                format!("{name} is a function\n{}", show::function(&def).trim_end()),
                "function",
            );
            found = true;
        }
        if (all || !found) && lookup(name).is_some() && super::enabled(shell, name) {
            let kind = if is_special(name) && shell.posix() {
                "special shell builtin"
            } else {
                "shell builtin"
            };
            say(&mut text, name.to_string(), format!("{name} is a {kind}"), "builtin");
            found = true;
        }
    }
    if all || !found {
        let runs = |path: &str| {
            std::fs::metadata(crate::text::to_os(path)).is_ok_and(|m| m.is_file())
                && sys::executable(path)
        };
        let paths: Vec<String> = if name.contains('/') {
            if runs(name) { vec![name.to_string()] } else { Vec::new() }
        } else if all {
            let path = shell.get("PATH").unwrap_or_default();
            path.split(':')
                .map(|dir| {
                    format!(
                        "{}/{name}",
                        if dir.is_empty() { "." } else { dir.trim_end_matches('/') }
                    )
                })
                .filter(|p| runs(p))
                .collect()
        } else {
            // Outside POSIX mode, a file that is not executable is found
            // as running it finds it, as in bash; looking hashes nothing.
            let hashed = shell.hash.get(name).cloned();
            match hashed.or_else(|| shell.search_path(name, !shell.posix())) {
                Some(path) => vec![path],
                None => Vec::new(),
            }
        };
        for path in paths {
            let hashed = !all && shell.hash.get(name) == Some(&path) && !name.contains('/');
            let verbose = if hashed {
                format!("{name} is hashed ({path})")
            } else {
                format!("{name} is {path}")
            };
            if how == Describe::Path {
                text.push_str(&format!("{path}\n"));
            } else {
                say(&mut text, path.clone(), verbose, "file");
            }
            found = true;
        }
    }
    out(shell, "type", &text) && found
}

pub fn type_(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "type", &args[1..], "afptP", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let how = if flags.has('t') {
        Describe::Word
    } else if flags.has('p') || flags.has('P') {
        Describe::Path
    } else {
        Describe::Verbose
    };
    let mut status = 0;
    for name in &flags.rest {
        let found = describe_all(shell, name, how, flags.has('P'), flags.has('a'), flags.has('f'));
        if !found {
            if how == Describe::Verbose {
                shell.error(&format!("type: {name}: not found"));
            }
            status = 1;
        }
    }
    Ok(status)
}

pub fn hash(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "hash", &args[1..], "dlp:rt", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('r') {
        shell.hash.clear();
    }
    if let Some(path) = flags.value('p') {
        if shell.opts.restricted {
            shell.error("hash: restricted: cannot use -p");
            return Ok(1);
        }
        for name in &flags.rest {
            shell.hash.insert(name.clone(), path.to_string());
        }
        return Ok(0);
    }
    if flags.rest.is_empty() {
        if flags.has('r') {
            return Ok(0);
        }
        if shell.hash.is_empty() {
            // On standard output, as bash says it.
            let empty = !shell.posix() && !out(shell, "hash", "hash: hash table empty\n");
            return Ok(i32::from(empty));
        }
        let mut names: Vec<(&String, &String)> = shell.hash.iter().collect();
        names.sort();
        let mut text = String::new();
        if flags.has('l') {
            for (name, path) in names {
                let _ = writeln!(text, "builtin hash -p {path} {name}");
            }
        } else {
            text.push_str("hits\tcommand\n");
            for (name, path) in names {
                let hits = shell.hash_hits.get(name.as_str()).copied().unwrap_or(0);
                let _ = writeln!(text, "{hits:4}\t{path}");
            }
        }
        return Ok(i32::from(!out(shell, "hash", &text)));
    }
    let mut status = 0;
    let mut text = String::new();
    for name in &flags.rest {
        if flags.has('d') {
            if shell.hash.remove(name.as_str()).is_none() {
                shell.error(&format!("hash: {name}: not found"));
                status = 1;
            }
            continue;
        }
        if flags.has('t') {
            match shell.hash.get(name.as_str()) {
                Some(path) if flags.rest.len() > 1 => text.push_str(&format!("{name}\t{path}\n")),
                Some(path) => text.push_str(&format!("{path}\n")),
                None => {
                    shell.error(&format!("hash: {name}: not found"));
                    status = 1;
                }
            }
            continue;
        }
        if name.contains('/')
            || lookup(name).is_some()
            || shell.functions.contains_key(name.as_str())
        {
            continue;
        }
        let saved = shell.opts.hashall;
        shell.opts.hashall = true;
        let found = shell.find_command(name);
        shell.opts.hashall = saved;
        if found.is_none() {
            shell.error(&format!("hash: {name}: not found"));
            status = 1;
        }
    }
    Ok(if out(shell, "hash", &text) { status } else { 1 })
}

pub fn trap(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "trap", &args[1..], "lp", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('l') {
        return Ok(i32::from(!out(shell, "trap", &trap::table())));
    }
    if flags.has('p') || flags.rest.is_empty() {
        let slots: Vec<usize> = if flags.rest.is_empty() {
            (0..trap::SLOTS).collect()
        } else {
            let mut slots = Vec::new();
            for name in &flags.rest {
                match trap::parse(name, true) {
                    Some(slot) => slots.push(slot),
                    None => {
                        shell.error(&format!("trap: {name}: invalid signal specification"));
                        return Ok(1);
                    }
                }
            }
            slots
        };
        let traps = shell.parent_traps.as_ref().unwrap_or(&shell.traps);
        let mut text = String::new();
        for slot in slots {
            if let Some(command) = traps.get(slot).and_then(|t| t.as_deref()) {
                text.push_str(&format!(
                    "trap -- {} {}\n",
                    quote_trap(command),
                    trap::slot_name(slot)
                ));
            } else if flags.has('p') && !flags.rest.is_empty() {
                // `trap -p SIG` for one at its default: nothing, as bash.
            }
        }
        return Ok(i32::from(!out(shell, "trap", &text)));
    }
    // `trap action sig...`, or `trap sig...` (one sig, or a number first)
    // to reset. A subshell's own traps are listed from then on.
    shell.parent_traps = None;
    let rest = &flags.rest;
    let first = &rest[0];
    let reset_all = rest.len() == 1 && trap::parse(first, true).is_some()
        || first.chars().all(|c| c.is_ascii_digit()) && !first.is_empty();
    let (action, signals): (Option<&str>, &[String]) = if reset_all {
        (None, &rest[..])
    } else if first == "-" {
        (None, &rest[1..])
    } else {
        (Some(first.as_str()), &rest[1..])
    };
    if signals.is_empty() && action.is_some() {
        return usage(shell, "trap", "");
    }
    let mut status = 0;
    for name in signals {
        let Some(slot) = trap::parse(name, true) else {
            shell.error(&format!("trap: {name}: invalid signal specification"));
            status = 1;
            continue;
        };
        let is_signal = (1..=trap::MAX_SIGNAL).contains(&slot);
        // A signal ignored when a non-interactive shell started stays so.
        if is_signal && shell.ignored[slot] && !shell.interactive {
            continue;
        }
        // SIGKILL and SIGSTOP cannot be caught: their traps are kept, and
        // never run, as in dash and bash (POSIX leaves it undefined).
        shell.traps[slot] = action.map(str::to_string);
        if is_signal && !matches!(slot as i32, libc::SIGKILL | libc::SIGSTOP) {
            shell.apply_disposition(slot as i32);
        }
    }
    Ok(status)
}

fn quote_trap(command: &str) -> String {
    format!("'{}'", command.replace('\'', "'\\''"))
}

pub fn getopts(shell: &mut Shell, args: &[String]) -> Res {
    if args.len() < 3 {
        return usage(shell, "getopts", "");
    }
    let optstring = args[1].clone();
    let name = args[2].clone();
    let operands: Vec<String> =
        if args.len() > 3 { args[3..].to_vec() } else { shell.args.clone() };
    let silent = optstring.starts_with(':');
    let spec = optstring.strip_prefix(':').unwrap_or(&optstring);
    let report = !silent && shell.get("OPTERR").is_none_or(|v| v != "0");
    let mut optind =
        shell.get("OPTIND").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).max(1);
    // As in bash, a name that cannot be set fails the command once the
    // option is taken: the status is 1, and OPTIND has moved.
    let set_name = |shell: &mut Shell, value: &str| -> i32 {
        if !crate::parse::is_name(&name) {
            shell.error(&format!("getopts: `{name}': not a valid identifier"));
            return 1;
        }
        let _ = shell.assign(&name, value.to_string(), false);
        0
    };
    let end = |shell: &mut Shell, optind: usize| -> Res {
        shell.getopts_char = 0;
        shell.set_var("OPTIND", &optind.to_string());
        shell.unset("OPTARG");
        set_name(shell, "?");
        Ok(1)
    };
    if shell.getopts_char == 0 {
        let Some(arg) = operands.get(optind - 1) else { return end(shell, optind) };
        if arg == "--" {
            return end(shell, optind + 1);
        }
        if !arg.starts_with('-') || arg == "-" {
            return end(shell, optind);
        }
        shell.getopts_char = 1;
    }
    let arg: Vec<char> = operands.get(optind - 1).map_or_default(|a| a.chars().collect());
    let Some(&letter) = arg.get(shell.getopts_char) else {
        shell.getopts_char = 0;
        return end(shell, optind + 1);
    };
    shell.getopts_char += 1;
    let at_end = shell.getopts_char >= arg.len();
    if at_end {
        optind += 1;
        shell.getopts_char = 0;
    }
    let known = letter != ':' && spec.contains(letter);
    let takes =
        known && spec[spec.find(letter).unwrap_or(0) + letter.len_utf8()..].starts_with(':');
    let program = shell.arg0.clone();
    if !known {
        if report {
            let _ = sys::write_all(
                2,
                crate::text::encode(&format!("{program}: illegal option -- {letter}\n")).as_slice(),
            );
        }
        if silent {
            let _ = shell.assign("OPTARG", letter.to_string(), false);
        } else {
            shell.unset("OPTARG");
        }
        shell.set_var("OPTIND", &optind.to_string());
        return Ok(set_name(shell, "?"));
    }
    if takes {
        if !at_end {
            let value: String = arg[shell.getopts_char..].iter().collect();
            optind += 1;
            shell.getopts_char = 0;
            let _ = shell.assign("OPTARG", value, false);
        } else if let Some(value) = operands.get(optind - 1) {
            let _ = shell.assign("OPTARG", value.clone(), false);
            optind += 1;
        } else {
            shell.set_var("OPTIND", &optind.to_string());
            if silent {
                let _ = shell.assign("OPTARG", letter.to_string(), false);
                return Ok(set_name(shell, ":"));
            }
            if report {
                let line = format!("{program}: option requires an argument -- {letter}\n");
                let _ = sys::write_all(2, crate::text::encode(&line).as_slice());
            }
            shell.unset("OPTARG");
            return Ok(set_name(shell, "?"));
        }
    } else {
        shell.unset("OPTARG");
    }
    shell.set_var("OPTIND", &optind.to_string());
    Ok(set_name(shell, &letter.to_string()))
}

/// `ulimit`'s resources: letter, resource, unit and its size, description.
// The resources are i32 on Radix and u32 on glibc.
#[allow(clippy::unnecessary_cast)]
const LIMITS: &[(char, i32, Option<&str>, u64, &str)] = &[
    ('R', libc::RLIMIT_RTTIME as i32, Some("microseconds"), 1, "real-time non-blocking time"),
    ('c', libc::RLIMIT_CORE as i32, Some("blocks"), 1024, "core file size"),
    ('d', libc::RLIMIT_DATA as i32, Some("kbytes"), 1024, "data seg size"),
    ('e', libc::RLIMIT_NICE as i32, None, 1, "scheduling priority"),
    ('f', libc::RLIMIT_FSIZE as i32, Some("blocks"), 1024, "file size"),
    ('i', libc::RLIMIT_SIGPENDING as i32, None, 1, "pending signals"),
    ('l', libc::RLIMIT_MEMLOCK as i32, Some("kbytes"), 1024, "max locked memory"),
    ('m', libc::RLIMIT_RSS as i32, Some("kbytes"), 1024, "max memory size"),
    ('n', libc::RLIMIT_NOFILE as i32, None, 1, "open files"),
    ('p', -1, Some("512 bytes"), 512, "pipe size"),
    ('q', libc::RLIMIT_MSGQUEUE as i32, Some("bytes"), 1, "POSIX message queues"),
    ('r', libc::RLIMIT_RTPRIO as i32, None, 1, "real-time priority"),
    ('s', libc::RLIMIT_STACK as i32, Some("kbytes"), 1024, "stack size"),
    ('t', libc::RLIMIT_CPU as i32, Some("seconds"), 1, "cpu time"),
    ('u', libc::RLIMIT_NPROC as i32, None, 1, "max user processes"),
    ('v', libc::RLIMIT_AS as i32, Some("kbytes"), 1024, "virtual memory"),
    ('x', libc::RLIMIT_LOCKS as i32, None, 1, "file locks"),
];

pub fn ulimit(shell: &mut Shell, args: &[String]) -> Res {
    let mut hard = false;
    let mut soft = false;
    let mut all = false;
    // Each resource asked for, with the value to set it to: the rest of
    // its word, or the next word unless that is an option, as in bash.
    let mut commands: Vec<(char, Option<String>)> = Vec::new();
    let mut i = 1;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(letters) = arg.strip_prefix('-').filter(|l| !l.is_empty()) else { break };
        i += 1;
        for (at, c) in letters.char_indices() {
            match c {
                'H' => hard = true,
                'S' => soft = true,
                'a' => all = true,
                c if LIMITS.iter().any(|l| l.0 == c) => {
                    let rest = &letters[at + c.len_utf8()..];
                    let value = if !rest.is_empty() {
                        Some(rest.to_string())
                    } else if let Some(next) = args.get(i).filter(|a| !a.starts_with('-')) {
                        i += 1;
                        Some(next.clone())
                    } else {
                        None
                    };
                    commands.push((c, value));
                    break;
                }
                c => return usage(shell, "ulimit", &format!("-{c}: invalid option")),
            }
        }
    }
    // An operand is the value of the last resource, or of -f.
    if let Some(arg) = args.get(i) {
        match commands.last_mut() {
            None => commands.push(('f', Some(arg.clone()))),
            Some((_, value @ None)) => *value = Some(arg.clone()),
            Some(_) => {}
        }
    }
    // POSIX counts file sizes in 512-byte blocks.
    let block = if shell.posix() { 512 } else { 1024 };
    let scale = |letter: char, size: u64| if matches!(letter, 'c' | 'f') { block } else { size };
    // A resource's line when several are shown, as bash prints it.
    let described = |letter: char, shown: &str| {
        let Some((_, _, unit, _, description)) = LIMITS.iter().find(|l| l.0 == letter) else {
            return String::new();
        };
        let unit_text = match unit {
            Some(_) if matches!(letter, 'c' | 'f') => format!("(blocks, -{letter}) "),
            Some(unit) => format!("({unit}, -{letter}) "),
            None => format!("(-{letter}) "),
        };
        format!("{description:<20} {unit_text:>20}{shown}\n")
    };
    if all {
        let mut text = String::new();
        for (letter, resource, _, size, _) in LIMITS {
            let shown = show_limit(*resource, hard && !soft, scale(*letter, *size));
            text.push_str(&described(*letter, &shown));
        }
        return Ok(i32::from(!out(shell, "ulimit", &text)));
    }
    if commands.is_empty() {
        commands.push(('f', None));
    }
    let several = commands.len() > 1;
    for (letter, value) in &commands {
        let Some((_, resource, _, size, description)) = LIMITS.iter().find(|l| l.0 == *letter)
        else {
            continue;
        };
        let Some(value) = value else {
            let shown = show_limit(*resource, hard && !soft, scale(*letter, *size));
            let text = if several { described(*letter, &shown) } else { format!("{shown}\n") };
            if !out(shell, "ulimit", &text) {
                return Ok(1);
            }
            continue;
        };
        if *resource < 0 {
            shell.error(&format!("ulimit: {description}: cannot modify limit: Invalid argument"));
            return Ok(1);
        }
        let mut limit = match sys::getrlimit(*resource) {
            Ok(limit) => limit,
            Err(error) => {
                shell.error(&format!("ulimit: cannot get limit: {}", sys::describe(&error)));
                return Ok(1);
            }
        };
        // A decimal number, as bash takes it, or a word.
        let new = match value.as_str() {
            "unlimited" => libc::RLIM_INFINITY,
            "hard" => limit.rlim_max,
            "soft" => limit.rlim_cur,
            text => match text.parse::<u64>() {
                Ok(n) if text.bytes().all(|b| b.is_ascii_digit()) => {
                    n.saturating_mul(scale(*letter, *size)) as libc::rlim_t
                }
                _ => {
                    let kind = if text.starts_with("0x") || text.starts_with("0X") {
                        "hex number"
                    } else {
                        "number"
                    };
                    shell.error(&format!("ulimit: {text}: invalid {kind}"));
                    return Ok(1);
                }
            },
        };
        // Neither -H nor -S sets both.
        if hard || !soft {
            limit.rlim_max = new;
        }
        if soft || !hard {
            limit.rlim_cur = new;
        }
        if let Err(error) = sys::setrlimit(*resource, &limit) {
            shell.error(&format!(
                "ulimit: {description}: cannot modify limit: {}",
                sys::describe(&error)
            ));
            return Ok(1);
        }
    }
    Ok(0)
}

fn show_limit(resource: i32, hard: bool, size: u64) -> String {
    if resource < 0 {
        // The pipe size as bash shows it: PIPE_BUF, the most a write puts
        // in a pipe at once (4096 on Radix as on Linux), in 512-byte
        // blocks.
        return (4096 / size).to_string();
    }
    match sys::getrlimit(resource) {
        Ok(limit) => {
            let value = if hard { limit.rlim_max } else { limit.rlim_cur };
            if value == libc::RLIM_INFINITY {
                "unlimited".into()
            } else {
                (value / size).to_string()
            }
        }
        Err(_) => "unknown".into(),
    }
}

pub fn umask(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "umask", &args[1..], "pS", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let current = sys::umask(0o022);
    sys::umask(current);
    let Some(mode) = flags.rest.first() else {
        let text = if flags.has('S') { symbolic_mask(current) } else { format!("{current:04o}") };
        let text = if flags.has('p') {
            format!("umask{} {text}\n", if flags.has('S') { " -S" } else { "" })
        } else {
            format!("{text}\n")
        };
        return Ok(i32::from(!out(shell, "umask", &text)));
    };
    // A mode that starts with a digit is octal, as in bash.
    let new = if mode.starts_with(|c: char| c.is_ascii_digit()) {
        match u32::from_str_radix(mode, 8) {
            Ok(n) if n <= 0o7777 => n,
            _ => {
                shell.error(&format!("umask: {mode}: octal number out of range"));
                return Ok(1);
            }
        }
    } else {
        match apply_symbolic(current, mode) {
            Ok(mask) => mask,
            Err(message) => {
                shell.error(&format!("umask: {message}"));
                return Ok(1);
            }
        }
    };
    sys::umask(new & 0o777);
    if flags.has('S') {
        return Ok(i32::from(!out(shell, "umask", &format!("{}\n", symbolic_mask(new)))));
    }
    Ok(0)
}

/// A mask as `umask -S` shows it: the permissions it leaves.
fn symbolic_mask(mask: u32) -> String {
    let allowed = !mask & 0o777;
    let part = |shift: u32, who: char| {
        let bits = (allowed >> shift) & 7;
        let mut s = format!("{who}=");
        if bits & 4 != 0 {
            s.push('r');
        }
        if bits & 2 != 0 {
            s.push('w');
        }
        if bits & 1 != 0 {
            s.push('x');
        }
        s
    };
    format!("{},{},{}", part(6, 'u'), part(3, 'g'), part(0, 'o'))
}

/// A symbolic mode (`u=rwx,g+w,o-r`) applied to the permissions a mask
/// leaves: the new mask, or bash's error, which names the character that
/// is wrong (NUL for a missing operator at the end).
fn apply_symbolic(mask: u32, mode: &str) -> Result<u32, String> {
    let mut allowed = !mask & 0o777;
    for clause in mode.split(',') {
        let mut chars = clause.chars().peekable();
        let mut who = 0u32;
        while let Some(&c) = chars.peek() {
            match c {
                'u' => who |= 0o700,
                'g' => who |= 0o070,
                'o' => who |= 0o007,
                'a' => who |= 0o777,
                _ => break,
            }
            chars.next();
        }
        if who == 0 {
            who = 0o777;
        }
        let op = chars.next().unwrap_or('\0');
        if !matches!(op, '+' | '-' | '=') {
            return Err(format!("`{op}': invalid symbolic mode operator"));
        }
        let mut perms = 0u32;
        for c in chars {
            perms |= match c {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                _ => return Err(format!("`{c}': invalid symbolic mode character")),
            };
        }
        let bits = perms & who;
        match op {
            '+' => allowed |= bits,
            '-' => allowed &= !bits,
            _ => allowed = (allowed & !who) | bits,
        }
    }
    Ok(!allowed & 0o777)
}

pub fn times(shell: &mut Shell, _: &[String]) -> Res {
    let (user, system, child_user, child_system) = sys::times();
    let mmss = |t: f64| {
        let minutes = (t / 60.0).floor();
        format!("{}m{:.3}s", minutes as i64, t - minutes * 60.0)
    };
    let text =
        format!("{} {}\n{} {}\n", mmss(user), mmss(system), mmss(child_user), mmss(child_system));
    Ok(i32::from(!out(shell, "times", &text)))
}

pub fn caller(shell: &mut Shell, args: &[String]) -> Res {
    let calls: Vec<_> = shell.frames.iter().rev().cloned().collect();
    match args.get(1) {
        None => {
            let Some(frame) = calls.first() else { return Ok(1) };
            let file = calls.get(1).map_or_else(|| shell.arg0.clone(), |f| f.file.to_string());
            out(shell, "caller", &format!("{} {file}\n", frame.call_line));
            Ok(0)
        }
        Some(n) => {
            let Ok(n) = n.parse::<usize>() else {
                shell.error(&format!("caller: {n}: invalid number"));
                return Ok(1);
            };
            let Some(frame) = calls.get(n) else { return Ok(1) };
            let function = calls.get(n + 1).map_or("main".to_string(), |f| f.name.clone());
            let file = calls.get(n + 1).map_or_else(|| shell.arg0.clone(), |f| f.file.to_string());
            out(shell, "caller", &format!("{} {function} {file}\n", frame.call_line));
            Ok(0)
        }
    }
}

pub fn enable(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "enable", &args[1..], "anpsf:d", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('f') || flags.has('d') {
        shell.error("enable: loadable builtins are not supported");
        return Ok(1);
    }
    if flags.rest.is_empty() {
        let mut text = String::new();
        for builtin in TABLE {
            let on = super::enabled(shell, builtin.name);
            if flags.has('s') && !is_special(builtin.name) {
                continue;
            }
            if flags.has('a') || (flags.has('n') && !on) || (!flags.has('n') && on) {
                text.push_str(&format!("enable {}{}\n", if on { "" } else { "-n " }, builtin.name));
            }
        }
        return Ok(i32::from(!out(shell, "enable", &text)));
    }
    let mut status = 0;
    for name in &flags.rest {
        if lookup(name).is_none() {
            shell.error(&format!("enable: {name}: not a shell builtin"));
            status = 1;
            continue;
        }
        if flags.has('n') {
            shell.disabled.insert(name.clone());
        } else {
            shell.disabled.remove(name);
        }
    }
    Ok(status)
}

pub fn help(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "help", &args[1..], "dms", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let mut text = String::new();
    if flags.rest.is_empty() {
        text.push_str(&format!(
            "Nukesh {}, the shell of Dukovo.\nThe builtins, and what they take; `help name` for one, man nukesh for all.\n\n",
            env!("CARGO_PKG_VERSION")
        ));
        for builtin in TABLE {
            text.push_str(&format!(" {}\n", builtin.usage));
        }
        return Ok(i32::from(!out(shell, "help", &text)));
    }
    let mut status = 0;
    for pattern in &flags.rest {
        let matcher = crate::pattern::Pattern::new(pattern, crate::pattern::Options::default());
        let found: Vec<_> = TABLE
            .iter()
            .filter(|b| matcher.matches(b.name) || b.name.starts_with(pattern.as_str()))
            .collect();
        if found.is_empty() {
            shell.error(&format!("help: no help topics match `{pattern}'.  Try `man nukesh'."));
            status = 1;
            continue;
        }
        for builtin in found {
            if flags.has('s') {
                text.push_str(&format!("{}: {}\n", builtin.name, builtin.usage));
            } else if flags.has('d') {
                text.push_str(&format!("{} - {}\n", builtin.name, builtin.summary));
            } else {
                text.push_str(&format!(
                    "{}: {}\n    {}\n",
                    builtin.name, builtin.usage, builtin.summary
                ));
            }
        }
    }
    Ok(if out(shell, "help", &text) { status } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbolic_masks() {
        assert_eq!(symbolic_mask(0o022), "u=rwx,g=rx,o=rx");
        assert_eq!(apply_symbolic(0o022, "g+w"), Ok(0o002));
        assert_eq!(apply_symbolic(0o022, "u=rwx,go="), Ok(0o077));
        assert_eq!(apply_symbolic(0o022, "a-x"), Ok(0o133));
        assert_eq!(apply_symbolic(0o022, "u"), Err("`\0': invalid symbolic mode operator".into()));
        assert_eq!(
            apply_symbolic(0o022, "u+s"),
            Err("`s': invalid symbolic mode character".into())
        );
    }

    #[test]
    fn the_table_is_sorted() {
        assert!(TABLE.windows(2).all(|w| w[0].name < w[1].name));
    }
}
