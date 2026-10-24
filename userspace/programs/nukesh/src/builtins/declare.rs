// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Variables and options: `declare` and its kin (`typeset`, `local`,
//! `export`, `readonly`), `unset`, `set` and `shopt`.

use super::{Flags, flags, out, usage};
use crate::expand::{attribute_flags, quote};
use crate::shell::{Flow, OPTION_TABLE, Opts, Res, SHOPTS, Shell};
use crate::vars::{Value, attr};
use crate::{show, text};

/// An argument of a declaration builtin.
#[derive(Clone, Debug)]
pub enum DeclArg {
    /// An option, a name, or `name=value`.
    Plain(String),
    /// `name=(...)`, its elements expanded.
    Array { name: String, append: bool, elements: Vec<(Option<String>, String)> },
}

/// Which declaration builtin runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Declare,
    Local,
    Export,
    Readonly,
}

/// The attribute letters and their bits.
const LETTERS: &[(char, u16)] = &[
    ('a', attr::ARRAY),
    ('A', attr::ASSOC),
    ('i', attr::INTEGER),
    ('l', attr::LOWER),
    ('n', attr::NAMEREF),
    ('r', attr::READONLY),
    ('t', attr::TRACE),
    ('u', attr::UPPER),
    ('x', attr::EXPORT),
];

pub fn declare(shell: &mut Shell, args: &[String]) -> Res {
    run_declaration(shell, &args[0], args[1..].iter().cloned().map(DeclArg::Plain).collect())
}

pub fn local(shell: &mut Shell, args: &[String]) -> Res {
    declare(shell, args)
}

pub fn export(shell: &mut Shell, args: &[String]) -> Res {
    declare(shell, args)
}

pub fn readonly(shell: &mut Shell, args: &[String]) -> Res {
    declare(shell, args)
}

/// Runs `declare`, `typeset`, `local`, `export` or `readonly`.
pub fn run_declaration(shell: &mut Shell, name: &str, args: Vec<DeclArg>) -> Res {
    let kind = match name {
        "local" => Kind::Local,
        "export" => Kind::Export,
        "readonly" => Kind::Readonly,
        _ => Kind::Declare,
    };
    let spec = match kind {
        Kind::Declare | Kind::Local => "aAfFgiIlnprtux",
        Kind::Export => "fnp",
        Kind::Readonly => "aAfp",
    };
    // The options come first, as plain words.
    let mut options = Vec::new();
    let mut i = 0;
    while let Some(DeclArg::Plain(word)) = args.get(i) {
        if word == "--" {
            i += 1;
            break;
        }
        if word.len() > 1 && (word.starts_with('-') || word.starts_with('+')) && !word.contains('=')
        {
            options.push(word.clone());
            i += 1;
        } else {
            break;
        }
    }
    let operands = &args[i..];
    let flags = match flags(shell, name, &options, spec, true) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if kind == Kind::Local && shell.vars.function_depth() == 0 {
        shell.error("local: can only be used in a function");
        return Ok(1);
    }

    // What to set and take away.
    let mut set = 0u16;
    let mut clear = 0u16;
    for (letter, bit) in LETTERS {
        if flags.has(*letter) {
            set |= bit;
        }
        if flags.has_off(*letter) {
            clear |= bit;
        }
    }
    match kind {
        Kind::Export if flags.has('n') => {
            set &= !attr::NAMEREF;
            clear |= attr::EXPORT;
        }
        Kind::Export => set |= attr::EXPORT,
        Kind::Readonly => set |= attr::READONLY,
        _ => {}
    }
    let functions = flags.has('f') || flags.has('F');

    // Printing.
    let print = flags.has('p') || operands.is_empty();
    if print && operands.is_empty() {
        if functions {
            return Ok(print_functions(shell, &flags, set));
        }
        return Ok(print_variables(shell, kind, &flags, set));
    }
    if flags.has('p') {
        let mut status = 0;
        let mut text = String::new();
        for arg in operands {
            let DeclArg::Plain(var) = arg else { continue };
            if functions {
                if shell.functions.contains_key(var.as_str()) {
                    text.push_str(&function_text(shell, var, flags.has('F')));
                } else {
                    status = 1;
                }
                continue;
            }
            match declare_line(shell, var) {
                Some(line) => {
                    text.push_str(&line);
                    text.push('\n');
                }
                None => {
                    shell.error(&format!("{}: {var}: not found", kind_name(kind, name)));
                    status = 1;
                }
            }
        }
        return Ok(if out(shell, "declare", &text) { status } else { 1 });
    }

    // Functions: `declare -f name` prints them; export and readonly mark
    // them.
    if functions {
        let mut status = 0;
        let mut text = String::new();
        for arg in operands {
            let DeclArg::Plain(name) = arg else { continue };
            if !shell.functions.contains_key(name.as_str()) {
                if kind != Kind::Declare {
                    shell.error(&format!("{name}: not a function"));
                }
                status = 1;
                continue;
            }
            match kind {
                Kind::Export => {
                    if flags.has('n') {
                        shell.exported_functions.remove(name);
                    } else {
                        shell.exported_functions.insert(name.clone());
                    }
                }
                Kind::Readonly => {
                    shell.readonly_functions.insert(name.clone());
                }
                _ if set & attr::EXPORT != 0 => {
                    shell.exported_functions.insert(name.clone());
                }
                _ if set & attr::READONLY != 0 => {
                    shell.readonly_functions.insert(name.clone());
                }
                // With extdebug, where it was defined too, as in bash
                // (which names -c's text "environment").
                _ if flags.has('F') && shell.shopt("extdebug") => {
                    let def = &shell.functions[name.as_str()];
                    let file = if shell.source_flag == Some('c') && *def.file == *shell.arg0 {
                        "environment"
                    } else {
                        &def.file
                    };
                    text.push_str(&format!("{name} {} {file}\n", def.line));
                }
                _ if flags.has('F') => text.push_str(&format!("{name}\n")),
                _ => text.push_str(&show::function(&shell.functions[name.as_str()])),
            }
        }
        return Ok(if out(shell, "declare", &text) { status } else { 1 });
    }

    let scope = if flags.has('g') || kind == Kind::Export || kind == Kind::Readonly {
        Scope::Global
    } else if shell.vars.function_depth() > 0 {
        Scope::Local { inherit: flags.has('I') || shell.shopt("localvar_inherit") }
    } else {
        Scope::Global
    };
    // `-g` is the global variable even where a local one hides it, as in
    // bash; export and readonly take the nearest.
    let hidden = flags.has('g').then(|| shell.vars.hide_locals());
    let mut status = 0;
    let mut result = Ok(());
    for arg in operands {
        // `declare 'a[$i]=x'`: the subscript is expanded, as in bash.
        let mut arg = std::borrow::Cow::Borrowed(arg);
        if let DeclArg::Plain(word) = &*arg
            && !matches!(kind, Kind::Export | Kind::Readonly)
        {
            let end = word.find('=').unwrap_or(word.len());
            let lhs = word[..end].strip_suffix('+').unwrap_or(&word[..end]);
            match shell.expand_element(lhs) {
                Ok(element) if element != lhs => {
                    arg = std::borrow::Cow::Owned(DeclArg::Plain(format!(
                        "{element}{}",
                        &word[lhs.len()..]
                    )));
                }
                Ok(_) => {}
                Err(flow) => {
                    result = Err(flow);
                    break;
                }
            }
        }
        if let Err(message) = declare_one(shell, kind, scope, set, clear, &arg) {
            if !message.is_empty() {
                shell.error(&format!("{}: {message}", kind_name(kind, name)));
            }
            status = 1;
            // Assigning a read-only variable ends a POSIX script, with
            // dash's status.
            if shell.posix() && !shell.interactive && message.ends_with("readonly variable") {
                result = Err(Flow::Fatal(2));
                break;
            }
        }
    }
    if let Some(hidden) = hidden {
        shell.vars.reveal(hidden);
    }
    result.map(|()| status)
}

fn kind_name(kind: Kind, given: &str) -> &str {
    match kind {
        Kind::Local => "local",
        Kind::Export => "export",
        Kind::Readonly => "readonly",
        Kind::Declare => given,
    }
}

#[derive(Clone, Copy)]
enum Scope {
    Global,
    /// In the innermost function; with `inherit`, starting with the
    /// value and attributes it has outside.
    Local {
        inherit: bool,
    },
}

/// Declares one name.
fn declare_one(
    shell: &mut Shell,
    kind: Kind,
    scope: Scope,
    set: u16,
    clear: u16,
    arg: &DeclArg,
) -> Result<(), String> {
    let (name, index, value) = match arg {
        DeclArg::Array { name, append, elements } => {
            (name.clone(), None, Some(Assignment::Array(*append, elements.clone())))
        }
        DeclArg::Plain(word) => {
            let (lhs, value) = match word.find('=') {
                Some(eq) => {
                    let (lhs, append) = match word[..eq].strip_suffix('+') {
                        Some(lhs) => (lhs, true),
                        None => (&word[..eq], false),
                    };
                    (lhs.to_string(), Some(Assignment::Scalar(append, word[eq + 1..].to_string())))
                }
                None => (word.clone(), None),
            };
            let (name, index) = match lhs.find('[') {
                // export and readonly take names only, as in bash.
                Some(_) if matches!(kind, Kind::Export | Kind::Readonly) => {
                    return Err(format!("`{lhs}': not a valid identifier"));
                }
                Some(open) if lhs.ends_with(']') => {
                    (lhs[..open].to_string(), Some(lhs[open + 1..lhs.len() - 1].to_string()))
                }
                _ => (lhs, None),
            };
            (name, index, value)
        }
    };
    if name == "-" && kind == Kind::Local {
        // `local -`: the options come back when the function returns.
        if let Some(frame) = shell.frames.iter_mut().rev().find(|f| f.name != "source")
            && frame.saved_opts.is_none()
        {
            frame.saved_opts = Some(Box::new(shell.opts.clone()));
        }
        return Ok(());
    }
    if !crate::parse::is_name(&name) {
        return Err(format!(
            "`{}': not a valid identifier",
            match arg {
                DeclArg::Plain(word) => word.as_str(),
                DeclArg::Array { name, .. } => name.as_str(),
            }
        ));
    }
    if set & attr::NAMEREF != 0 {
        if let Some(Assignment::Scalar(_, target)) = &value
            && !target.is_empty()
            && !crate::parse::is_name(target.split('[').next().unwrap_or(""))
        {
            return Err(format!("`{target}': invalid variable name for name reference"));
        }
        if let Some(Assignment::Scalar(_, target)) = &value
            && *target == name
        {
            return Err(format!("{name}: nameref variable self references not allowed"));
        }
    }

    // The variable itself (a reference is followed unless it is the
    // reference that is being declared or changed).
    let own = set & attr::NAMEREF != 0 || clear & attr::NAMEREF != 0;
    let target = if own { name.clone() } else { shell.resolve(&name) };
    let readonly_now = shell.vars.get(&target).is_some_and(|v| v.has(attr::READONLY));
    if readonly_now && (value.is_some() || clear & attr::READONLY != 0) {
        return Err(format!("{target}: readonly variable"));
    }
    match scope {
        Scope::Local { inherit } => {
            if !shell.vars.is_local(&target) {
                shell.vars.local(&target, inherit);
            }
        }
        Scope::Global => {
            // An existing variable keeps its scope; a new one is global.
            if shell.vars.get(&target).is_none() {
                shell.vars.global(&target);
            }
        }
    }
    let var = shell.vars.get_mut(&target).ok_or_else(String::new)?;
    let was = var.attrs;
    // Arrays: `-a` makes a string element 0; `-A` makes it key `0`.
    // One declared but not yet given a value converts no more than one
    // that has one.
    if set & attr::ASSOC != 0 {
        if matches!(var.value, Some(Value::Array(_))) || var.value.is_none() && var.has(attr::ARRAY)
        {
            return Err(format!("{target}: cannot convert indexed to associative array"));
        }
        if let Some(Value::Str(s)) = var.value.take() {
            var.value = Some(Value::Assoc([("0".to_string(), s)].into_iter().collect()));
        }
    } else if set & attr::ARRAY != 0 {
        if matches!(var.value, Some(Value::Assoc(_))) || var.value.is_none() && var.has(attr::ASSOC)
        {
            return Err(format!("{target}: cannot convert associative to indexed array"));
        }
        if let Some(Value::Str(s)) = var.value.take() {
            var.value = Some(Value::Array([(0, s)].into_iter().collect()));
        }
    }
    let mut attrs = (var.attrs | (set & !attr::READONLY)) & !clear;
    // `-l` and `-u` undo each other.
    if set & attr::LOWER != 0 {
        attrs &= !attr::UPPER;
    }
    if set & attr::UPPER != 0 {
        attrs &= !attr::LOWER;
    }
    var.attrs = attrs;
    let is_array = matches!(var.value, Some(Value::Array(_) | Value::Assoc(_)))
        || attrs & (attr::ARRAY | attr::ASSOC) != 0;
    // A reference holds a name: assigning it does not go through it.
    if attrs & attr::NAMEREF != 0
        && let Some(Assignment::Scalar(_, target_name)) = &value
    {
        var.value = Some(Value::Str(target_name.clone()));
    } else {
        match value {
            Some(Assignment::Scalar(append, text)) => {
                let result = match (&index, is_array) {
                    (Some(index), _) => shell.assign_element(&target, index, text, append),
                    // `declare -a a='(1 2)'`: the old way to give an array.
                    (None, true) if text.starts_with('(') && text.ends_with(')') && !append => {
                        let elements = compound_elements(shell, &text[1..text.len() - 1])
                            .map_err(|_| String::new())?;
                        shell.assign_array(&target, elements, false)
                    }
                    (None, true) => shell.assign_element(&target, "0", text, append),
                    (None, false) => shell.assign(&target, text, append),
                };
                if let Err(message) = result {
                    if let Some(var) = shell.vars.get_mut(&target) {
                        var.attrs = was;
                    }
                    return Err(message);
                }
            }
            Some(Assignment::Array(append, elements)) => {
                shell.assign_array(&target, elements, append)?
            }
            None => {}
        }
    }
    if set & attr::READONLY != 0
        && let Some(var) = shell.vars.get_mut(&target)
    {
        var.attrs |= attr::READONLY;
    }
    // `export name` of a variable with no value marks it to be exported
    // once it gets one.
    Ok(())
}

enum Assignment {
    Scalar(bool, String),
    Array(bool, Vec<(Option<String>, String)>),
}

/// The elements of `(...)` given as text.
fn compound_elements(
    shell: &mut Shell,
    inner: &str,
) -> Result<Vec<(Option<String>, String)>, Flow> {
    let words = match crate::parse::words(inner, shell.parse_options()) {
        Ok(words) => words,
        Err(error) => {
            shell.error(&error.message());
            return Err(Flow::Fatal(1));
        }
    };
    let mut elements = Vec::new();
    for word in words {
        for field in shell.expand_words(std::slice::from_ref(&word))? {
            // `[key]=value`.
            if let Some(rest) = field.strip_prefix('[')
                && let Some(close) = rest.find("]=")
            {
                elements.push((Some(rest[..close].to_string()), rest[close + 2..].to_string()));
            } else {
                elements.push((None, field));
            }
        }
    }
    Ok(elements)
}

/// `declare -p name`'s line for a variable: `None` if there is none.
pub fn declare_line(shell: &Shell, name: &str) -> Option<String> {
    let var = shell.vars.get(name)?;
    let mut flags = attribute_flags(var.attrs);
    let value = var.value.as_ref();
    if let Some(Value::Array(_)) = value
        && !flags.contains('a')
    {
        flags.insert(0, 'a');
    }
    if let Some(Value::Assoc(_)) = value
        && !flags.contains('A')
    {
        flags.insert(0, 'A');
    }
    let flags = if flags.is_empty() { "--".to_string() } else { format!("-{flags}") };
    let posix_kind = if shell.posix() {
        if var.has(attr::READONLY) {
            Some("readonly")
        } else if var.has(attr::EXPORT) {
            Some("export")
        } else {
            None
        }
    } else {
        None
    };
    let head = match posix_kind {
        Some(kind) => format!("{kind} {name}"),
        None => format!("declare {flags} {name}"),
    };
    Some(match value {
        None => head,
        Some(Value::Str(s)) => format!("{head}={}", double_quote(s)),
        Some(Value::Array(map)) => {
            let items: Vec<String> =
                map.iter().map(|(k, v)| format!("[{k}]={}", double_quote(v))).collect();
            format!("{head}=({})", items.join(" "))
        }
        Some(Value::Assoc(map)) => {
            let mut items = String::new();
            for (k, v) in map {
                items.push_str(&format!("[{}]={} ", quote_key(k), double_quote(v)));
            }
            format!("{head}=({items})")
        }
    })
}

/// A value as `declare -p` writes it: in double quotes, or as `$'...'`
/// if it holds control characters.
pub fn double_quote(s: &str) -> String {
    if s.chars().any(|c| c.is_control() || text::char_byte(c).is_some()) {
        return crate::expand::quote_always(s);
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// An associative array's key as bash writes one: as it is, unless it
/// holds what the shell would read otherwise or is `@` or `*`.
pub fn quote_key(key: &str) -> String {
    if key.is_empty() || key == "@" || key == "*" || crate::expand::has_shell_metas(key) {
        double_quote(key)
    } else if key.chars().any(|c| c.is_control() || text::char_byte(c).is_some()) {
        crate::expand::quote_always(key)
    } else {
        key.to_string()
    }
}

/// `declare -f`, `declare -F` and their kin with no names.
/// `declare -f` and `-F`, `export -f` and `readonly -f` with no names: the
/// functions with the attributes asked for (`set`), each with its text
/// unless `-F`, and a `declare` line that gives it its attributes back.
fn print_functions(shell: &Shell, flags: &Flags, set: u16) -> i32 {
    let mut names: Vec<&String> = shell.functions.keys().collect();
    names.sort();
    let mut text = String::new();
    for name in names {
        let marked = (set & attr::EXPORT == 0 || shell.exported_functions.contains(name))
            && (set & attr::READONLY == 0 || shell.readonly_functions.contains(name));
        if marked {
            text.push_str(&function_text(shell, name, flags.has('F')));
        }
    }
    i32::from(!out(shell, "declare", &text))
}

/// A function as `declare -f` shows it: its text, then a `declare -frx`
/// line if it has attributes; with `names_only`, that line alone.
fn function_text(shell: &Shell, name: &str, names_only: bool) -> String {
    let mut letters = String::new();
    if shell.readonly_functions.contains(name) {
        letters.push('r');
    }
    if shell.exported_functions.contains(name) {
        letters.push('x');
    }
    let line = format!("declare -f{letters} {name}\n");
    if names_only {
        return line;
    }
    let mut text = show::function(&shell.functions[name]);
    if !letters.is_empty() {
        text.push_str(&line);
    }
    text
}

/// `declare` (and `export`, `readonly`) with no names: the variables,
/// those with the attributes asked for.
fn print_variables(shell: &Shell, kind: Kind, flags: &Flags, set: u16) -> i32 {
    let mut text = String::new();
    let want = set & !attr::NAMEREF
        | if flags.has('n') && kind != Kind::Export { attr::NAMEREF } else { 0 };
    let plain = kind == Kind::Declare && !flags.has('p') && want == 0;
    for (name, var) in shell.vars.all() {
        if want != 0 && var.attrs & want != want {
            continue;
        }
        if plain {
            if let Some(value) = &var.value {
                text.push_str(&set_line(name, value));
                text.push('\n');
            }
            continue;
        }
        if let Some(line) = declare_line(shell, name) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    i32::from(!out(shell, "declare", &text))
}

/// A variable as `set` prints it: `name=value`, quoted to be read back.
fn set_line(name: &str, value: &Value) -> String {
    match value {
        Value::Str(s) => format!("{name}={}", quote(s)),
        Value::Array(map) => {
            let items: Vec<String> =
                map.iter().map(|(k, v)| format!("[{k}]={}", double_quote(v))).collect();
            format!("{name}=({})", items.join(" "))
        }
        Value::Assoc(map) => {
            let mut items = String::new();
            for (k, v) in map {
                items.push_str(&format!("[{}]={} ", quote_key(k), double_quote(v)));
            }
            format!("{name}=({items})")
        }
    }
}

pub fn unset(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "unset", &args[1..], "fvn", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    let mut status = 0;
    for name in &flags.rest {
        if flags.has('f') {
            if shell.readonly_functions.contains(name) {
                shell.error(&format!("unset: {name}: cannot unset: readonly function"));
                status = 1;
                continue;
            }
            shell.functions.remove(name.as_str());
            shell.exported_functions.remove(name);
            continue;
        }
        let base = name.split('[').next().unwrap_or("");
        if !crate::parse::is_name(base) || (name.contains('[') && !name.ends_with(']')) {
            shell.error(&format!("unset: `{name}': not a valid identifier"));
            status = 1;
            continue;
        }
        if flags.has('n') {
            // The reference itself, not what it names.
            if !shell.vars.unset(name) {
                shell.error(&format!("unset: {name}: cannot unset: readonly variable"));
                status = 1;
            }
            continue;
        }
        let exists = shell.vars.get(&shell.resolve(base)).is_some();
        if !exists
            && !flags.has('v')
            && !name.contains('[')
            && shell.functions.contains_key(name.as_str())
        {
            shell.functions.remove(name.as_str());
            continue;
        }
        if !shell.unset_checked(name)? {
            status = 1;
            if shell.posix() && !shell.interactive {
                return Err(Flow::Fatal(2));
            }
        }
    }
    Ok(status)
}

pub fn set(shell: &mut Shell, args: &[String]) -> Res {
    let args = &args[1..];
    if args.is_empty() {
        let mut text = String::new();
        for (name, var) in shell.vars.all() {
            if let Some(value) = &var.value {
                text.push_str(&set_line(name, value));
                text.push('\n');
            }
        }
        if !shell.posix() {
            let mut names: Vec<&String> = shell.functions.keys().collect();
            names.sort();
            for name in names {
                text.push_str(&show::function(&shell.functions[name.as_str()]));
            }
        }
        return Ok(i32::from(!out(shell, "set", &text)));
    }
    let mut i = 0;
    let mut params: Option<Vec<String>> = None;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            params = Some(args[i + 1..].to_vec());
            break;
        }
        if arg == "-" {
            shell.opts.xtrace = false;
            shell.opts.verbose = false;
            if i + 1 < args.len() {
                params = Some(args[i + 1..].to_vec());
            }
            break;
        }
        let on = match arg.chars().next() {
            Some('-') if arg.len() > 1 => true,
            Some('+') if arg.len() > 1 => false,
            _ => {
                params = Some(args[i..].to_vec());
                break;
            }
        };
        for letter in arg.chars().skip(1) {
            if letter == 'o' {
                match args.get(i + 1) {
                    Some(name) if !name.starts_with('-') && !name.starts_with('+') => {
                        i += 1;
                        if let Err(status) = set_option(shell, name, on) {
                            return Ok(status);
                        }
                    }
                    _ => {
                        if !print_options(shell, on) {
                            return Ok(1);
                        }
                    }
                }
                continue;
            }
            let name = match letter {
                'i' => {
                    shell.error(&format!("set: {}i: invalid option", if on { '-' } else { '+' }));
                    return usage(shell, "set", "");
                }
                'r' if on => {
                    shell.opts.restricted = true;
                    continue;
                }
                'r' => continue,
                'H' => "histexpand",
                _ => match Opts::by_letter(letter) {
                    Some(name) => name,
                    None => {
                        let _ = usage(
                            shell,
                            "set",
                            &format!("{}{letter}: invalid option", if on { '-' } else { '+' }),
                        );
                        return Ok(2);
                    }
                },
            };
            if let Err(status) = set_option(shell, name, on) {
                return Ok(status);
            }
        }
        i += 1;
    }
    if let Some(params) = params {
        shell.args = params;
    }
    Ok(0)
}

/// `set -o name` and `set +o name`.
pub fn set_option(shell: &mut Shell, name: &str, on: bool) -> Result<(), i32> {
    if name == "history" || name == "histexpand" {
        shell.history_option(name, on);
        return Ok(());
    }
    if name == "posix" && on {
        // As in bash, which leaves it on when POSIX mode goes off.
        shell.shopts.insert("inherit_errexit", true);
    }
    if name == "restricted" || (name == "privileged" && on) {
        return Ok(());
    }
    match shell.opts.get_mut(name) {
        Some(option) => *option = on,
        None => {
            shell.error(&format!("set: {name}: invalid option name"));
            return Err(2);
        }
    }
    match (name, on) {
        ("vi", true) => shell.opts.emacs = false,
        ("emacs", true) => shell.opts.vi = false,
        ("monitor", _) => shell.monitor_changed(),
        _ => {}
    }
    Ok(())
}

/// `set -o` and `set +o` with no name.
fn print_options(shell: &Shell, minus: bool) -> bool {
    let mut names: Vec<&str> = OPTION_TABLE.iter().map(|(name, _)| *name).collect();
    names.extend(["history", "histexpand"]);
    names.sort();
    let mut text = String::new();
    for name in names {
        let on = match name {
            "history" | "histexpand" => shell.history_option_on(name),
            _ => shell.opts.get(name).unwrap_or(false),
        };
        if minus {
            text.push_str(&format!("{name:<15}\t{}\n", if on { "on" } else { "off" }));
        } else {
            text.push_str(&format!("set {}o {name}\n", if on { '-' } else { '+' }));
        }
    }
    out(shell, "set", &text)
}

pub fn shopt(shell: &mut Shell, args: &[String]) -> Res {
    let flags = match flags(shell, "shopt", &args[1..], "pqsuo", false) {
        Ok(flags) => flags,
        Err(status) => return Ok(status),
    };
    if flags.has('s') && flags.has('u') {
        shell.error("shopt: cannot set and unset shell options simultaneously");
        return Ok(1);
    }
    let set_o = flags.has('o');
    let names: Vec<String> = if flags.rest.is_empty() {
        if set_o {
            OPTION_TABLE.iter().map(|(n, _)| n.to_string()).collect()
        } else {
            SHOPTS.iter().map(|(n, _)| n.to_string()).collect()
        }
    } else {
        flags.rest.clone()
    };
    let get = |shell: &Shell, name: &str| -> Option<bool> {
        if set_o { shell.opts.get(name) } else { shell.shopts.get(name).copied() }
    };
    let mut status = 0;
    if flags.has('s') || flags.has('u') {
        if flags.rest.is_empty() {
            // `shopt -s` alone lists the options that are on.
            return Ok(list_shopts(
                shell,
                &names,
                set_o,
                flags.has('s'),
                flags.has('p'),
                flags.has('q'),
            ));
        }
        let on = flags.has('s');
        for name in &names {
            if set_o {
                if let Err(code) = set_option(shell, name, on) {
                    status = code;
                }
                continue;
            }
            match SHOPTS.iter().find(|(n, _)| *n == name.as_str()) {
                Some((key, _)) if *key == "login_shell" || *key == "restricted_shell" => {}
                Some((key, _)) => {
                    shell.shopts.insert(key, on);
                }
                None => {
                    shell.error(&format!("shopt: {name}: invalid shell option name"));
                    status = 1;
                }
            }
        }
        return Ok(status);
    }
    let mut text = String::new();
    for name in &names {
        match get(shell, name) {
            Some(on) => {
                if !on {
                    status = 1;
                }
                if flags.has('q') {
                    continue;
                }
                if flags.has('p') {
                    let switch = if set_o {
                        if on { "set -o" } else { "set +o" }
                    } else if on {
                        "shopt -s"
                    } else {
                        "shopt -u"
                    };
                    text.push_str(&format!("{switch} {name}\n"));
                } else {
                    text.push_str(&format!("{name:<15}\t{}\n", if on { "on" } else { "off" }));
                }
            }
            None => {
                shell.error(&format!("shopt: {name}: invalid shell option name"));
                status = 1;
            }
        }
    }
    Ok(if out(shell, "shopt", &text) { status } else { 1 })
}

/// `shopt -s` or `shopt -u` with no names: the options in that state.
fn list_shopts(
    shell: &Shell,
    names: &[String],
    set_o: bool,
    on: bool,
    p: bool,
    quiet: bool,
) -> i32 {
    let mut text = String::new();
    for name in names {
        let state =
            if set_o { shell.opts.get(name) } else { shell.shopts.get(name.as_str()).copied() };
        if state != Some(on) {
            continue;
        }
        if p {
            text.push_str(&format!("shopt {} {name}\n", if on { "-s" } else { "-u" }));
        } else {
            text.push_str(&format!("{name:<15}\t{}\n", if on { "on" } else { "off" }));
        }
    }
    i32::from(!quiet && !out(shell, "shopt", &text))
}
