// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The syntax tree as shell text again: for `declare -f`, `type`, the
//! commands `jobs` lists, and `set -x` of compound commands.
//!
//! Functions come out laid out as bash lays them out, four spaces to a
//! level; everything else on one line.

use std::cell::RefCell;

use crate::ast::*;

thread_local! {
    /// Here-document bodies written out after the line their redirection
    /// is on.
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Ends a line of a function's text, and puts after it the bodies of the
/// here-documents on it.
fn newline(out: &mut String) {
    out.push('\n');
    PENDING.with(|pending| {
        for body in pending.borrow_mut().drain(..) {
            out.push_str(&body);
        }
    });
}

/// Forgets the bodies of a one-line text, which shows none.
fn drop_pending() {
    PENDING.with(|pending| pending.borrow_mut().clear());
}

/// A function definition, as `declare -f` prints it.
pub fn function(def: &FunctionDef) -> String {
    let mut out = format!("{} () ", def.name);
    newline(&mut out);
    command(&def.body, 0, &mut out);
    newline(&mut out);
    out
}

/// A command on one line.
pub fn one_line(cmd: &Command) -> String {
    let mut out = String::new();
    flat_command(cmd, &mut out);
    drop_pending();
    out
}

/// An and-or list on one line.
pub fn and_or_line(list: &AndOr) -> String {
    let mut out = String::new();
    flat_and_or(list, &mut out);
    drop_pending();
    out
}

/// A whole program on one line.
pub fn program_line(program: &Program) -> String {
    let mut out = String::new();
    flat_program(program, &mut out);
    drop_pending();
    out
}

/// A pipeline on one line.
pub fn pipeline_line(p: &Pipeline) -> String {
    let mut out = String::new();
    flat_pipeline(p, &mut out);
    drop_pending();
    out
}

fn indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

fn command(cmd: &Command, level: usize, out: &mut String) {
    match cmd {
        Command::Compound(compound, redirects) => {
            compound_multi(compound, level, out);
            for r in redirects {
                out.push(' ');
                redirect(r, out);
            }
        }
        other => flat_command(other, out),
    }
}

fn body(program: &Program, level: usize, out: &mut String) {
    for complete in &program.commands {
        for (and_or, sep) in &complete.items {
            indent(level, out);
            multi_and_or(and_or, level, out);
            out.push_str(match sep {
                Sep::Async => " &",
                Sep::Seq => ";",
            });
            newline(out);
        }
    }
}

fn multi_and_or(list: &AndOr, level: usize, out: &mut String) {
    multi_pipeline(&list.first, level, out);
    for (op, p) in &list.rest {
        out.push_str(match op {
            AndOrOp::And => " && ",
            AndOrOp::Or => " || ",
        });
        multi_pipeline(p, level, out);
    }
}

fn multi_pipeline(p: &Pipeline, level: usize, out: &mut String) {
    if let Some(posix) = p.time {
        out.push_str(if posix { "time -p " } else { "time " });
    }
    if p.bang {
        out.push_str("! ");
    }
    for (i, cmd) in p.commands.iter().enumerate() {
        if i > 0 {
            out.push_str(if p.pipe_stderr[i - 1] { " |& " } else { " | " });
        }
        command(cmd, level, out);
    }
}

fn compound_multi(compound: &CompoundCommand, level: usize, out: &mut String) {
    match compound {
        CompoundCommand::Brace(program) => {
            out.push_str("{ ");
            newline(out);
            body(program, level + 1, out);
            indent(level, out);
            out.push('}');
        }
        CompoundCommand::Subshell(program) => {
            out.push_str("( ");
            flat_program(program, out);
            out.push_str(" )");
        }
        CompoundCommand::If { arms, otherwise } => {
            for (i, (cond, then)) in arms.iter().enumerate() {
                out.push_str(if i == 0 { "if " } else { "elif " });
                flat_program(cond, out);
                out.push_str("; then");
                newline(out);
                body(then, level + 1, out);
                indent(level, out);
            }
            if let Some(otherwise) = otherwise {
                out.push_str("else");
                newline(out);
                body(otherwise, level + 1, out);
                indent(level, out);
            }
            out.push_str("fi");
        }
        CompoundCommand::While { cond, body: inner }
        | CompoundCommand::Until { cond, body: inner } => {
            out.push_str(if matches!(compound, CompoundCommand::While { .. }) {
                "while "
            } else {
                "until "
            });
            flat_program(cond, out);
            out.push_str("; do");
            newline(out);
            body(inner, level + 1, out);
            indent(level, out);
            out.push_str("done");
        }
        CompoundCommand::For { var, words, body: inner, .. }
        | CompoundCommand::Select { var, words, body: inner, .. } => {
            out.push_str(if matches!(compound, CompoundCommand::For { .. }) {
                "for "
            } else {
                "select "
            });
            out.push_str(var);
            if let Some(words) = words {
                out.push_str(" in");
                for w in words {
                    out.push(' ');
                    word(w, out);
                }
            }
            out.push(';');
            newline(out);
            out.push_str("do");
            newline(out);
            body(inner, level + 1, out);
            indent(level, out);
            out.push_str("done");
        }
        CompoundCommand::ArithFor { init, cond, step, body: inner, .. } => {
            out.push_str("for ((");
            raw_word(init, out);
            out.push_str("; ");
            raw_word(cond, out);
            out.push_str("; ");
            raw_word(step, out);
            out.push_str("))");
            newline(out);
            out.push_str("do");
            newline(out);
            body(inner, level + 1, out);
            indent(level, out);
            out.push_str("done");
        }
        CompoundCommand::Case { word: w, arms, .. } => {
            out.push_str("case ");
            word(w, out);
            out.push_str(" in ");
            newline(out);
            for arm in arms {
                indent(level + 1, out);
                for (i, pattern) in arm.patterns.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" | ");
                    }
                    word(pattern, out);
                }
                out.push(')');
                newline(out);
                body(&arm.body, level + 2, out);
                indent(level + 1, out);
                out.push_str(match arm.end {
                    CaseEnd::Break => ";;",
                    CaseEnd::FallThrough => ";&",
                    CaseEnd::Continue => ";;&",
                });
                newline(out);
            }
            indent(level, out);
            out.push_str("esac");
        }
        other => flat_compound(other, out),
    }
}

fn flat_program(program: &Program, out: &mut String) {
    let mut first = true;
    for complete in &program.commands {
        for (and_or, sep) in &complete.items {
            if !first {
                out.push(' ');
            }
            first = false;
            flat_and_or(and_or, out);
            if *sep == Sep::Async {
                out.push_str(" &");
            } else {
                out.push(';');
            }
        }
    }
    if out.ends_with(';') {
        out.pop();
    }
}

fn flat_and_or(list: &AndOr, out: &mut String) {
    flat_pipeline(&list.first, out);
    for (op, p) in &list.rest {
        out.push_str(match op {
            AndOrOp::And => " && ",
            AndOrOp::Or => " || ",
        });
        flat_pipeline(p, out);
    }
}

fn flat_pipeline(p: &Pipeline, out: &mut String) {
    if let Some(posix) = p.time {
        out.push_str(if posix { "time -p " } else { "time " });
    }
    if p.bang {
        out.push_str("! ");
    }
    for (i, cmd) in p.commands.iter().enumerate() {
        if i > 0 {
            out.push_str(if p.pipe_stderr[i - 1] { " |& " } else { " | " });
        }
        flat_command(cmd, out);
    }
}

fn flat_command(cmd: &Command, out: &mut String) {
    match cmd {
        Command::Simple(simple) => {
            let mut first = true;
            let mut sep = |out: &mut String| {
                if !first {
                    out.push(' ');
                }
                first = false;
            };
            for a in &simple.assigns {
                sep(out);
                assign(a, out);
            }
            for w in &simple.words {
                sep(out);
                word(w, out);
            }
            for r in &simple.redirects {
                sep(out);
                redirect(r, out);
            }
        }
        Command::Compound(compound, redirects) => {
            flat_compound(compound, out);
            for r in redirects {
                out.push(' ');
                redirect(r, out);
            }
        }
        Command::Function(def) => {
            out.push_str(&def.name);
            out.push_str(" () ");
            flat_command(&def.body, out);
        }
    }
}

fn flat_compound(compound: &CompoundCommand, out: &mut String) {
    match compound {
        CompoundCommand::Brace(program) => {
            out.push_str("{ ");
            flat_program(program, out);
            out.push_str("; }");
        }
        CompoundCommand::Subshell(program) => {
            out.push('(');
            flat_program(program, out);
            out.push(')');
        }
        CompoundCommand::If { arms, otherwise } => {
            for (i, (cond, then)) in arms.iter().enumerate() {
                out.push_str(if i == 0 { "if " } else { " elif " });
                flat_program(cond, out);
                out.push_str("; then ");
                flat_program(then, out);
                out.push(';');
            }
            if let Some(otherwise) = otherwise {
                out.push_str(" else ");
                flat_program(otherwise, out);
                out.push(';');
            }
            out.push_str(" fi");
        }
        CompoundCommand::While { cond, body } | CompoundCommand::Until { cond, body } => {
            out.push_str(if matches!(compound, CompoundCommand::While { .. }) {
                "while "
            } else {
                "until "
            });
            flat_program(cond, out);
            out.push_str("; do ");
            flat_program(body, out);
            out.push_str("; done");
        }
        CompoundCommand::For { var, words, body, .. }
        | CompoundCommand::Select { var, words, body, .. } => {
            out.push_str(if matches!(compound, CompoundCommand::For { .. }) {
                "for "
            } else {
                "select "
            });
            out.push_str(var);
            if let Some(words) = words {
                out.push_str(" in");
                for w in words {
                    out.push(' ');
                    word(w, out);
                }
            }
            out.push_str("; do ");
            flat_program(body, out);
            out.push_str("; done");
        }
        CompoundCommand::ArithFor { init, cond, step, body, .. } => {
            out.push_str("for ((");
            raw_word(init, out);
            out.push_str("; ");
            raw_word(cond, out);
            out.push_str("; ");
            raw_word(step, out);
            out.push_str(")); do ");
            flat_program(body, out);
            out.push_str("; done");
        }
        CompoundCommand::Case { word: w, arms, .. } => {
            out.push_str("case ");
            word(w, out);
            out.push_str(" in");
            for arm in arms {
                out.push(' ');
                for (i, pattern) in arm.patterns.iter().enumerate() {
                    if i > 0 {
                        out.push('|');
                    }
                    word(pattern, out);
                }
                out.push_str(") ");
                flat_program(&arm.body, out);
                out.push_str(match arm.end {
                    CaseEnd::Break => ";;",
                    CaseEnd::FallThrough => ";&",
                    CaseEnd::Continue => ";;&",
                });
            }
            out.push_str(" esac");
        }
        CompoundCommand::Cond(expr, _) => {
            out.push_str("[[ ");
            cond(expr, out);
            out.push_str(" ]]");
        }
        CompoundCommand::Arith(w, _) => {
            out.push_str("((");
            raw_word(w, out);
            out.push_str("))");
        }
    }
}

fn cond(expr: &CondExpr, out: &mut String) {
    match expr {
        CondExpr::And(a, b) => {
            cond(a, out);
            out.push_str(" && ");
            cond(b, out);
        }
        CondExpr::Or(a, b) => {
            cond(a, out);
            out.push_str(" || ");
            cond(b, out);
        }
        CondExpr::Not(a) => {
            out.push_str("! ");
            cond(a, out);
        }
        CondExpr::Unary(op, w) => {
            out.push_str(op);
            out.push(' ');
            word(w, out);
        }
        CondExpr::Binary(a, op, b) => {
            word(a, out);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            word(b, out);
        }
        CondExpr::Word(w) => word(w, out),
    }
}

fn assign(a: &Assign, out: &mut String) {
    out.push_str(&a.name);
    if let Some(index) = &a.index {
        out.push('[');
        raw_word(index, out);
        out.push(']');
    }
    out.push_str(if a.append { "+=" } else { "=" });
    match &a.value {
        AssignValue::Scalar(w) => word(w, out),
        AssignValue::Array(elements) => {
            out.push('(');
            for (i, (key, w)) in elements.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                if let Some(key) = key {
                    out.push('[');
                    raw_word(key, out);
                    out.push_str("]=");
                }
                word(w, out);
            }
            out.push(')');
        }
    }
}

fn redirect(r: &Redirect, out: &mut String) {
    match &r.fd {
        Some(RedirFd::Number(n)) => out.push_str(&n.to_string()),
        Some(RedirFd::Var(name)) => {
            out.push('{');
            out.push_str(name);
            out.push('}');
        }
        None => {}
    }
    // As bash lays them out: `> file`, `2>&1`, `0<> file`.
    if r.op == RedirOp::ReadWrite && r.fd.is_none() {
        out.push('0');
    }
    out.push_str(match r.op {
        RedirOp::Read => "<",
        RedirOp::Write => ">",
        RedirOp::Clobber => ">|",
        RedirOp::Append => ">>",
        RedirOp::ReadWrite => "<>",
        RedirOp::DupRead => "<&",
        RedirOp::DupWrite => ">&",
        RedirOp::HereDoc => "<<",
        RedirOp::HereString => "<<<",
        RedirOp::WriteBoth => "&>",
        RedirOp::AppendBoth => "&>>",
    });
    match &r.target {
        RedirTarget::Word(w) => {
            if !matches!(r.op, RedirOp::DupRead | RedirOp::DupWrite) {
                out.push(' ');
            }
            word(w, out);
        }
        RedirTarget::HereDoc(doc) => {
            let doc = doc.borrow();
            let mut text = String::new();
            heredoc_text(&doc.body, doc.expand, &mut text);
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            // A delimiter that no line of the body is.
            let mut delimiter = "EOF".to_string();
            while text.lines().any(|line| line == delimiter) {
                delimiter.push('_');
            }
            if doc.expand {
                out.push_str(&delimiter);
            } else {
                out.push_str(&format!("'{delimiter}'"));
            }
            text.push_str(&delimiter);
            text.push('\n');
            PENDING.with(|pending| pending.borrow_mut().push(text));
        }
    }
}

/// A here-document's body as it would be written: in a body that is
/// expanded, `$`, `` ` `` and `\` that stand for themselves are escaped.
fn heredoc_text(body: &Word, expand: bool, out: &mut String) {
    for part in &body.parts {
        match part {
            WordPart::Lit(text) | WordPart::Quoted(text) if expand => {
                for c in text.chars() {
                    if matches!(c, '$' | '`' | '\\') {
                        out.push('\\');
                    }
                    out.push(c);
                }
            }
            WordPart::Lit(text) | WordPart::Quoted(text) => out.push_str(text),
            WordPart::Escaped(c) => {
                out.push('\\');
                out.push(*c);
            }
            WordPart::Double(parts) => heredoc_text(&Word { parts: parts.clone() }, expand, out),
            other => word(&Word { parts: vec![other.clone()] }, out),
        }
    }
}

/// A word as it would be written.
pub fn word(w: &Word, out: &mut String) {
    for part in &w.parts {
        word_part(part, false, out);
    }
}

pub fn word_text(w: &Word) -> String {
    let mut out = String::new();
    word(w, &mut out);
    out
}

/// A word whose parts are read as in double quotes (arithmetic), with no
/// quotes added.
fn raw_word(w: &Word, out: &mut String) {
    for part in &w.parts {
        match part {
            WordPart::Quoted(s) | WordPart::Lit(s) => out.push_str(s),
            other => word_part(other, true, out),
        }
    }
}

fn word_part(part: &WordPart, in_double: bool, out: &mut String) {
    match part {
        WordPart::Lit(s) => out.push_str(s),
        WordPart::Escaped(c) => {
            out.push('\\');
            out.push(*c);
        }
        WordPart::Quoted(s) => {
            if in_double {
                for c in s.chars() {
                    if matches!(c, '$' | '`' | '"' | '\\') {
                        out.push('\\');
                    }
                    out.push(c);
                }
            } else {
                out.push_str(&crate::expand::quote_always(s));
            }
        }
        WordPart::Double(parts) => {
            out.push('"');
            for part in parts {
                word_part(part, true, out);
            }
            out.push('"');
        }
        WordPart::Param(p) => param(p, out),
        WordPart::Command(program) => {
            out.push_str("$(");
            flat_program(program, out);
            out.push(')');
        }
        WordPart::Arith(w) => {
            out.push_str("$((");
            raw_word(w, out);
            out.push_str("))");
        }
        WordPart::ProcSub(output, program) => {
            out.push_str(if *output { ">(" } else { "<(" });
            flat_program(program, out);
            out.push(')');
        }
        WordPart::ArrayAssign(a) => assign(a, out),
    }
}

fn param(p: &ParamExp, out: &mut String) {
    let name = match &p.name {
        ParamName::Var(name) => name.clone(),
        ParamName::Positional(n) => n.to_string(),
        ParamName::Special(c) => c.to_string(),
    };
    let simple = p.index.is_none() && matches!(p.op, ParamOp::None) && !p.indirect;
    if simple && !matches!(p.name, ParamName::Positional(n) if n > 9) {
        out.push('$');
        out.push_str(&name);
        return;
    }
    out.push_str("${");
    if matches!(p.op, ParamOp::Length) {
        out.push('#');
    }
    if p.indirect || matches!(p.op, ParamOp::Names(_) | ParamOp::Keys(_)) {
        out.push('!');
    }
    out.push_str(&name);
    if let Some(index) = &p.index {
        out.push('[');
        raw_word(index, out);
        out.push(']');
    }
    let operand = |op: &str, w: &Word, out: &mut String| {
        out.push_str(op);
        word(w, out);
    };
    match &p.op {
        ParamOp::None | ParamOp::Length => {}
        ParamOp::Default(colon, w) => operand(if *colon { ":-" } else { "-" }, w, out),
        ParamOp::Assign(colon, w) => operand(if *colon { ":=" } else { "=" }, w, out),
        ParamOp::Error(colon, w) => operand(if *colon { ":?" } else { "?" }, w, out),
        ParamOp::Alternative(colon, w) => operand(if *colon { ":+" } else { "+" }, w, out),
        ParamOp::RemovePrefix(longest, w) => operand(if *longest { "##" } else { "#" }, w, out),
        ParamOp::RemoveSuffix(longest, w) => operand(if *longest { "%%" } else { "%" }, w, out),
        ParamOp::Substring(offset, length) => {
            out.push(':');
            raw_word(offset, out);
            if let Some(length) = length {
                out.push(':');
                raw_word(length, out);
            }
        }
        ParamOp::Replace(kind, pattern, replacement) => {
            out.push_str(match kind {
                ReplaceKind::First => "/",
                ReplaceKind::All => "//",
                ReplaceKind::Prefix => "/#",
                ReplaceKind::Suffix => "/%",
            });
            word(pattern, out);
            if let Some(replacement) = replacement {
                out.push('/');
                word(replacement, out);
            }
        }
        ParamOp::Case(upper, all, pattern) => {
            let c = if *upper { '^' } else { ',' };
            out.push(c);
            if *all {
                out.push(c);
            }
            if let Some(pattern) = pattern {
                word(pattern, out);
            }
        }
        ParamOp::Toggle(all, pattern) => {
            out.push_str(if *all { "~~" } else { "~" });
            if let Some(pattern) = pattern {
                word(pattern, out);
            }
        }
        ParamOp::Transform(t) => {
            out.push('@');
            out.push(*t);
        }
        ParamOp::Names(all) => out.push(if *all { '@' } else { '*' }),
        ParamOp::Keys(all) => out.push_str(if *all { "[@]" } else { "[*]" }),
    }
    out.push('}');
}
