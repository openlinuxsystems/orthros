// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The rules a set of modules must follow beyond the grammar.

use std::collections::{HashMap, HashSet};

use crate::{Abi, Call, Error, Field, ItemKind, Member, Module, Prim, Type};

/// Most words of a system call's arguments or results: ten registers.
pub const SYSCALL_WORDS: usize = 10;
/// Most words of a method's arguments or results: a message's data words.
pub const METHOD_WORDS: usize = 8;

/// Words Rust, C and the language reserve that may not name things.
const RESERVED: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "try", "auto", "case", "char", "default",
    "double", "float", "goto", "int", "long", "register", "short", "signed", "sizeof", "switch",
    "typedef", "union", "unsigned", "void", "volatile", "errno",
];

struct Checker<'a> {
    abi: &'a Abi,
    problems: Vec<Error>,
}

impl Checker<'_> {
    fn fail(&mut self, module: &Module, line: usize, message: String) {
        self.problems.push(Error { path: module.path.clone(), line, message });
    }

    fn doc(&mut self, module: &Module, line: usize, doc: &[String], what: &str) {
        if doc.iter().all(|line| line.trim().is_empty()) {
            self.fail(module, line, format!("{what} has no documentation comment"));
        }
    }

    fn name(&mut self, module: &Module, line: usize, name: &str, what: &str) {
        let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid {
            self.fail(module, line, format!("{what} `{name}` must be in snake_case"));
        }
        if RESERVED.contains(&name) {
            self.fail(module, line, format!("{what} `{name}` is a reserved word in Rust or C"));
        }
    }

    fn members(&mut self, module: &Module, members: &[Member], what: &str, limit: u64) {
        let mut names = HashSet::new();
        let mut values = HashSet::new();
        for member in members {
            self.doc(module, member.line, &member.doc, &format!("{what} `{}`", member.name));
            self.name(module, member.line, &member.name, what);
            if !names.insert(member.name.as_str()) {
                self.fail(
                    module,
                    member.line,
                    format!("{what} `{}` is defined twice", member.name),
                );
            }
            if !values.insert(member.value) {
                self.fail(
                    module,
                    member.line,
                    format!("{what} value {} is used twice", member.value),
                );
            }
            if member.value > limit {
                self.fail(
                    module,
                    member.line,
                    format!("{what} `{}`: {} is larger than {limit}", member.name, member.value),
                );
            }
        }
    }

    /// Checks that `ty` names something usable in a parameter list.
    fn param_type(&mut self, module: &Module, field: &Field) {
        match &field.ty {
            Type::Named(name) => match self.abi.find(name).map(|(_, item)| &item.kind) {
                Some(ItemKind::Enum { .. } | ItemKind::Flags { .. }) => {}
                Some(_) => self.fail(
                    module,
                    field.line,
                    format!("parameter `{}`: only enumerations and flags pass by name", field.name),
                ),
                None => {
                    self.fail(module, field.line, format!("unknown type `{name}`"));
                }
            },
            Type::Array(inner, len) => {
                if **inner != Type::Prim(Prim::U64) || *len == 0 {
                    self.fail(
                        module,
                        field.line,
                        format!("parameter `{}`: arrays of parameters are [u64; N]", field.name),
                    );
                }
            }
            Type::Prim(Prim::U8 | Prim::U16) => self.fail(
                module,
                field.line,
                format!("parameter `{}`: use u32 or u64 for parameters", field.name),
            ),
            _ => {}
        }
    }

    fn call(
        &mut self,
        module: &Module,
        call: &Call,
        what: &str,
        limit: usize,
        errors: &HashSet<&str>,
    ) {
        self.doc(module, call.line, &call.doc, &format!("{what} `{}`", call.name));
        self.name(module, call.line, &call.name, what);
        for (list, side) in [(&call.args, "arguments"), (&call.results, "results")] {
            let mut names = HashSet::new();
            for field in list.iter() {
                self.name(module, field.line, &field.name, "parameter");
                if !names.insert(field.name.as_str()) {
                    self.fail(
                        module,
                        field.line,
                        format!("parameter `{}` is named twice", field.name),
                    );
                }
                self.param_type(module, field);
                if side == "results" && matches!(field.ty, Type::Bytes(_)) {
                    self.fail(module, field.line, "results cannot be buffers".into());
                }
            }
            let words: usize = list.iter().map(|field| self.abi.words(&field.ty)).sum();
            if words > limit {
                self.fail(
                    module,
                    call.line,
                    format!(
                        "{what} `{}`: its {side} take {words} words; at most {limit} fit",
                        call.name
                    ),
                );
            }
        }
        for raised in &call.raises {
            if !errors.contains(raised.as_str()) {
                self.fail(
                    module,
                    call.line,
                    format!("{what} `{}` raises unknown error `{raised}`", call.name),
                );
            }
        }
    }

    fn structure(&mut self, module: &Module, name: &str, fields: &[Field], line: usize) {
        let mut offset: usize = 0;
        let mut names = HashSet::new();
        for field in fields {
            self.doc(module, field.line, &field.doc, &format!("field `{name}.{}`", field.name));
            self.name(module, field.line, &field.name, "field");
            if !names.insert(field.name.as_str()) {
                self.fail(module, field.line, format!("field `{}` is defined twice", field.name));
            }
            let allowed = match &field.ty {
                Type::Prim(_) | Type::Slot | Type::Addr => true,
                Type::Array(inner, len) => *len > 0 && matches!(**inner, Type::Prim(_)),
                Type::Named(other) => {
                    matches!(
                        self.abi.find(other).map(|(_, item)| &item.kind),
                        Some(ItemKind::Struct { .. })
                    )
                }
                Type::Bytes(_) => false,
            };
            if !allowed {
                self.fail(
                    module,
                    field.line,
                    format!(
                        "field `{}`: structures hold primitives, arrays of them and other structures",
                        field.name
                    ),
                );
                continue;
            }
            let Some((size, align)) = self.abi.layout(&field.ty) else { continue };
            if !offset.is_multiple_of(align) {
                self.fail(
                    module,
                    field.line,
                    format!(
                        "field `{}` would be padded to offset {}; add the padding as a field",
                        field.name,
                        offset.next_multiple_of(align)
                    ),
                );
            }
            offset = offset.next_multiple_of(align) + size;
        }
        if let Some((size, _)) = self.abi.layout(&Type::Named(name.to_string()))
            && size != offset
        {
            self.fail(
                module,
                line,
                format!(
                    "structure `{name}` would be padded at its end from {offset} to {size} bytes"
                ),
            );
        }
    }
}

/// Checks `abi`, returning every problem found.
pub fn check(abi: &Abi) -> Vec<Error> {
    let mut checker = Checker { abi, problems: Vec::new() };
    let errors: HashSet<&str> = abi
        .error_sets()
        .into_iter()
        .flat_map(|(_, item)| match &item.kind {
            ItemKind::Errors { errors, .. } => errors.iter().map(|e| e.name.as_str()).collect(),
            _ => Vec::new(),
        })
        .collect();
    let kinds: HashSet<&str> = match abi.find("ObjectKind").map(|(_, item)| &item.kind) {
        Some(ItemKind::Enum { variants, .. }) => variants.iter().map(|v| v.name.as_str()).collect(),
        _ => HashSet::new(),
    };

    let mut modules = HashSet::new();
    let mut types: HashMap<&str, &str> = HashMap::new();
    let mut syscalls = HashMap::new();
    let mut interfaces = HashMap::new();
    for module in &abi.modules {
        if !modules.insert(module.name.as_str()) {
            checker.fail(module, 0, format!("module `{}` is defined twice", module.name));
        }
        let stem = std::path::Path::new(&module.path).file_stem().and_then(|s| s.to_str());
        if stem != Some(module.name.as_str()) {
            checker.fail(
                module,
                0,
                format!("module `{}` must be in {}.ridl", module.name, module.name),
            );
        }
        checker.name(module, 0, &module.name, "module");
        checker.doc(module, 0, &module.doc, &format!("module `{}`", module.name));
        let mut consts = HashSet::new();
        for item in &module.items {
            match &item.kind {
                ItemKind::Const { ty, value } => {
                    checker.doc(module, item.line, &item.doc, &format!("constant `{}`", item.name));
                    let valid = item
                        .name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                    if !valid {
                        checker.fail(
                            module,
                            item.line,
                            format!("constant `{}` must be in UPPER_CASE", item.name),
                        );
                    }
                    if !consts.insert(item.name.as_str()) {
                        checker.fail(
                            module,
                            item.line,
                            format!("constant `{}` is defined twice", item.name),
                        );
                    }
                    let bits = ty.size() * 8;
                    if *ty == Prim::Bool || (bits < 64 && *value >> bits != 0) {
                        checker.fail(
                            module,
                            item.line,
                            format!("constant `{}` does not fit a {}", item.name, ty.name()),
                        );
                    }
                }
                ItemKind::Enum { repr, variants } => {
                    type_name(&mut checker, module, item.line, &item.name, &mut types);
                    checker.doc(
                        module,
                        item.line,
                        &item.doc,
                        &format!("enumeration `{}`", item.name),
                    );
                    if matches!(repr, Prim::Bool | Prim::I32 | Prim::I64) {
                        checker.fail(module, item.line, "enumerations are unsigned".into());
                    }
                    let limit =
                        if repr.size() == 8 { u64::MAX } else { (1 << (repr.size() * 8)) - 1 };
                    checker.members(module, variants, "variant", limit);
                }
                ItemKind::Flags { repr, bits } => {
                    type_name(&mut checker, module, item.line, &item.name, &mut types);
                    checker.doc(module, item.line, &item.doc, &format!("flags `{}`", item.name));
                    if !matches!(repr, Prim::U32 | Prim::U64) {
                        checker.fail(module, item.line, "flags are u32 or u64".into());
                    }
                    checker.members(module, bits, "flag", repr.size() as u64 * 8 - 1);
                }
                ItemKind::Errors { domain, errors: list } => {
                    checker.doc(module, item.line, &item.doc, &format!("errors `{}`", item.name));
                    checker.name(module, item.line, &item.name, "error set");
                    if *domain == 0 || *domain > u64::from(u16::MAX) {
                        checker.fail(
                            module,
                            item.line,
                            "an error domain is from 1 to 65535".into(),
                        );
                    }
                    checker.members(module, list, "error", u64::from(u16::MAX));
                    for error in list {
                        if error.value == 0 {
                            checker.fail(module, error.line, "error code 0 means success".into());
                        }
                        let errno = error.errno.as_deref().unwrap_or("");
                        if crate::errno(errno).is_none() {
                            checker.fail(module, error.line, format!("unknown errno `{errno}`"));
                        }
                    }
                }
                ItemKind::Struct { fields } => {
                    type_name(&mut checker, module, item.line, &item.name, &mut types);
                    checker.doc(
                        module,
                        item.line,
                        &item.doc,
                        &format!("structure `{}`", item.name),
                    );
                    checker.structure(module, &item.name, fields, item.line);
                }
                ItemKind::Syscall(call) => {
                    if let Some(other) = syscalls.insert(call.number, call.name.clone()) {
                        checker.fail(
                            module,
                            call.line,
                            format!("system call number {} is taken by `{other}`", call.number),
                        );
                    }
                    checker.call(module, call, "system call", SYSCALL_WORDS, &errors);
                }
                ItemKind::Interface { id, version, kind, methods } => {
                    checker.doc(
                        module,
                        item.line,
                        &item.doc,
                        &format!("interface `{}`", item.name),
                    );
                    checker.name(module, item.line, &item.name, "interface");
                    if *id == 0 || *id > u64::from(u16::MAX) {
                        checker.fail(
                            module,
                            item.line,
                            "an interface number is from 1 to 65535".into(),
                        );
                    }
                    if let Some(other) = interfaces.insert(*id, item.name.clone()) {
                        checker.fail(
                            module,
                            item.line,
                            format!("interface number {id:#x} is taken by `{other}`"),
                        );
                    }
                    if *version == 0 || *version > u64::from(u32::MAX) {
                        checker.fail(module, item.line, "a version is from 1 to 2^32 - 1".into());
                    }
                    if let Some(kind) = kind
                        && !kinds.contains(kind.as_str())
                    {
                        checker.fail(module, item.line, format!("`{kind}` is not an ObjectKind"));
                    }
                    let mut numbers = HashMap::new();
                    for method in methods {
                        if method.number == 0 || method.number > u64::from(u16::MAX) {
                            checker.fail(
                                module,
                                method.line,
                                "a method number is from 1 to 65535".into(),
                            );
                        }
                        if let Some(other) = numbers.insert(method.number, method.name.clone()) {
                            checker.fail(
                                module,
                                method.line,
                                format!("method number {} is taken by `{other}`", method.number),
                            );
                        }
                        checker.call(module, method, "method", METHOD_WORDS, &errors);
                    }
                    if !methods.is_empty()
                        && !abi.syscalls().iter().any(|(_, call)| call.name == "cap_call")
                    {
                        checker.fail(
                            module,
                            item.line,
                            "methods need a `cap_call` system call".into(),
                        );
                    }
                }
            }
        }
    }
    let mut problems = checker.problems;
    problems.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    problems.dedup();
    problems
}

fn type_name<'a>(
    checker: &mut Checker<'_>,
    module: &'a Module,
    line: usize,
    name: &'a str,
    types: &mut HashMap<&'a str, &'a str>,
) {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
        checker.fail(module, line, format!("type `{name}` must be in CamelCase"));
    }
    if let Some(other) = types.insert(name, &module.name) {
        checker.fail(module, line, format!("type `{name}` is also defined in module `{other}`"));
    }
}
