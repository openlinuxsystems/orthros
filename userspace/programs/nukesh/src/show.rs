// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The syntax tree as shell text again: for `declare -f`, `type`, the
//! commands `jobs` lists, and `set -x` of compound commands.
//!
//! Functions and jobs come out laid out as bash lays them out, four
//! spaces to a level; everything else on one line. A job's text is laid
//! out as a function's is but for lists, which stay on one line, as bash
//! has them outside a function: `while :; do` and its `done` have lines
//! of their own, `{ a; b; }` does not.
//!
//! One difference from bash is deliberate: a simple command run as a
//! coprocess comes out without the array's name, `coproc cat`, which
//! reads back as it was, where bash's `coproc COPROC cat` would run
//! `COPROC`.

use std::cell::{Cell, RefCell};

use crate::ast::*;

thread_local! {
    /// Here-document bodies written out after the line their redirection
    /// is on.
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// A job's text is being laid out.
    static JOB: Cell<bool> = const { Cell::new(false) };
}

/// Lays out a job's text with `write`.
fn job(write: impl FnOnce(&mut String)) -> String {
    let mut out = String::new();
    let outer = JOB.replace(true);
    write(&mut out);
    JOB.set(outer);
    drop_pending();
    out
}

/// A command as the text of the job that runs it.
pub fn job_line(cmd: &Command) -> String {
    job(|out| command(cmd, 0, out))
}

/// A compound command, without redirections, as a job's text.
pub fn job_compound(compound: &CompoundCommand) -> String {
    job(|out| compound_multi(compound, 0, out))
}

/// An and-or list as a job's text.
pub fn job_and_or(list: &AndOr) -> String {
    job(|out| multi_and_or(list, 0, out))
}

/// A pipeline as a job's text.
pub fn job_pipeline(p: &Pipeline) -> String {
    job(|out| multi_pipeline(p, 0, out))
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
    function_body(&def.body, 0, &mut out);
    newline(&mut out);
    out
}

/// A function's body, after its name, on the lines after it: a group, as
/// bash shows every one, around any other compound command.
fn function_body(body: &Command, level: usize, out: &mut String) {
    newline(out);
    indent(level, out);
    if let Command::Compound(compound, ..) = body
        && matches!(**compound, CompoundCommand::Brace(_))
    {
        command(body, level, out);
    } else {
        out.push_str("{ ");
        newline(out);
        indent(level + 1, out);
        command(body, level + 1, out);
        newline(out);
        indent(level, out);
        out.push('}');
    }
}

/// Whether here-document bodies wait for the end of the line.
fn pending() -> bool {
    PENDING.with(|pending| !pending.borrow().is_empty())
}

/// A simple command on one line.
pub fn simple_line(simple: &SimpleCommand) -> String {
    let mut out = String::new();
    flat_simple(simple, &mut out);
    drop_pending();
    out
}

/// A compound command on one line, without redirections.
pub fn compound_line(compound: &CompoundCommand) -> String {
    let mut out = String::new();
    flat_compound(compound, &mut out);
    drop_pending();
    out
}

/// An and-or list on one line.
#[cfg(test)]
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
        // One inside a body, as bash shows it; in a job's text too, laid
        // out as a function is.
        Command::Function(def) => {
            out.push_str("function ");
            out.push_str(&def.name);
            out.push_str(" () ");
            let outer = JOB.replace(false);
            function_body(&def.body, level, out);
            JOB.set(outer);
        }
        Command::Coproc(coproc) => {
            coproc_head(coproc, out);
            command(&coproc.command, level, out);
        }
        Command::Simple(_) => flat_command(cmd, out),
    }
}

/// `coproc`, and the array's name before a compound command.
fn coproc_head(coproc: &Coproc, out: &mut String) {
    out.push_str("coproc ");
    if !matches!(coproc.command, Command::Simple(_)) {
        out.push_str(&coproc.name);
        out.push(' ');
    }
}

/// A body, a command a line, as bash shows it: each line indented to
/// `level` and ended, but a command after `&` goes on its line. A `;`
/// ends each command but the last of a group, a function or a case arm
/// (`!last_semicolon`), and but one with here-documents, whose bodies
/// follow its line, then a blank line.
fn body(program: &Program, level: usize, last_semicolon: bool, out: &mut String) {
    items(program, level, last_semicolon, false, out);
}

/// [`body`], or with `subshell` the inside of `( )`: on the line of the
/// `(`, the lines after it indented to its level, ending before the `)`.
fn items(program: &Program, level: usize, last_semicolon: bool, subshell: bool, out: &mut String) {
    if JOB.get() {
        job_items(program, level, last_semicolon, subshell, out);
        return;
    }
    let items: Vec<_> = program.commands.iter().flat_map(|c| &c.items).collect();
    let mut on_line = subshell;
    for (i, (and_or, sep)) in items.iter().enumerate() {
        let last = i + 1 == items.len();
        if !on_line {
            indent(level, out);
        }
        multi_and_or(and_or, level, out);
        let heredocs = pending();
        match sep {
            Sep::Async => out.push_str(" &"),
            Sep::Seq if heredocs || last && (subshell || !last_semicolon) => {}
            Sep::Seq => out.push(';'),
        }
        if *sep == Sep::Async && !last && !heredocs {
            out.push(' ');
            on_line = true;
            continue;
        }
        if last && subshell {
            break;
        }
        newline(out);
        if heredocs {
            out.push('\n');
        }
        on_line = false;
    }
}

/// [`items`] in a job's text: on one line, indented to `level` and ended
/// unless `inline` in a group or a subshell.
fn job_items(
    program: &Program,
    level: usize,
    last_semicolon: bool,
    inline: bool,
    out: &mut String,
) {
    let items: Vec<_> = program.commands.iter().flat_map(|c| &c.items).collect();
    if !inline {
        indent(level, out);
    }
    for (i, (and_or, sep)) in items.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        multi_and_or(and_or, level, out);
        match sep {
            Sep::Async => out.push_str(" &"),
            Sep::Seq if i + 1 == items.len() && !last_semicolon => {}
            Sep::Seq => out.push(';'),
        }
    }
    if !inline {
        newline(out);
    }
}

/// `if`, with its `elif`s shown as bash shows them: an `if` in an `else`.
fn if_multi(
    arms: &[(Program, Program)],
    otherwise: Option<&Program>,
    level: usize,
    out: &mut String,
) {
    let Some(((cond, then), rest)) = arms.split_first() else { return };
    out.push_str("if ");
    flat_program(cond, out);
    out.push_str("; then");
    newline(out);
    body(then, level + 1, true, out);
    if !rest.is_empty() {
        indent(level, out);
        out.push_str("else");
        newline(out);
        indent(level + 1, out);
        if_multi(rest, otherwise, level + 1, out);
        out.push(';');
        newline(out);
    } else if let Some(otherwise) = otherwise {
        indent(level, out);
        out.push_str("else");
        newline(out);
        body(otherwise, level + 1, true, out);
    }
    indent(level, out);
    out.push_str("fi");
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
            // `|&` as what it stands for, as bash shows it.
            out.push_str(if p.pipe_stderr[i - 1] { " 2>&1 | " } else { " | " });
        }
        command(cmd, level, out);
    }
}

fn compound_multi(compound: &CompoundCommand, level: usize, out: &mut String) {
    match compound {
        CompoundCommand::Brace(program) if JOB.get() => {
            out.push_str("{ ");
            job_items(program, level, true, true, out);
            out.push_str(" }");
        }
        CompoundCommand::Brace(program) => {
            out.push_str("{ ");
            newline(out);
            body(program, level + 1, false, out);
            indent(level, out);
            out.push('}');
        }
        CompoundCommand::Subshell(program, _) => {
            // As bash shows it: a command a line, at the subshell's level.
            out.push_str("( ");
            items(program, level, false, true, out);
            out.push_str(" )");
        }
        CompoundCommand::If { arms, otherwise } => if_multi(arms, otherwise.as_ref(), level, out),
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
            body(inner, level + 1, true, out);
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
            indent(level, out);
            out.push_str("do");
            newline(out);
            body(inner, level + 1, true, out);
            indent(level, out);
            out.push_str("done");
        }
        CompoundCommand::ArithFor { init, cond, step, body: inner, .. } => {
            out.push_str("for ((");
            arith_part(init, out);
            out.push_str("; ");
            arith_part(cond, out);
            out.push_str("; ");
            arith_part(step, out);
            out.push_str("))");
            newline(out);
            indent(level, out);
            out.push_str("do");
            newline(out);
            body(inner, level + 1, true, out);
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
                body(&arm.body, level + 2, false, out);
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
        Command::Simple(simple) => flat_simple(simple, out),
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
        Command::Coproc(coproc) => {
            coproc_head(coproc, out);
            flat_command(&coproc.command, out);
        }
    }
}

fn flat_simple(simple: &SimpleCommand, out: &mut String) {
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
    for (i, w) in simple.words.iter().enumerate() {
        sep(out);
        // A reserved word that a redirection before it kept from being one
        // (`<&- esac`) is escaped, as it comes first here.
        if i == 0 && w.as_literal().is_some_and(crate::parse::is_reserved) {
            out.push('\\');
        }
        word(w, out);
    }
    for r in &simple.redirects {
        sep(out);
        redirect(r, out);
    }
}

fn flat_compound(compound: &CompoundCommand, out: &mut String) {
    match compound {
        CompoundCommand::Brace(program) => {
            out.push_str("{ ");
            flat_program(program, out);
            out.push_str("; }");
        }
        CompoundCommand::Subshell(program, _) => {
            out.push_str("( ");
            flat_program(program, out);
            out.push_str(" )");
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
            arith_part(init, out);
            out.push_str("; ");
            arith_part(cond, out);
            out.push_str("; ");
            arith_part(step, out);
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
        // As bash shows it.
        CondExpr::Word(w) => {
            out.push_str("-n ");
            word(w, out);
        }
        CondExpr::Group(a) => {
            out.push_str("( ");
            cond(a, out);
            out.push_str(" )");
        }
    }
}

fn assign(a: &Assign, out: &mut String) {
    out.push_str(&a.name);
    if let Some(index) = &a.index {
        out.push('[');
        subscript_word(index, out);
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
                    subscript_word(key, out);
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
    // As bash lays them out: `> file`, `2>&1`, `0<> file`, `0<&3`, and a
    // close as `0>&-`.
    if r.fd.is_none() {
        match r.op {
            RedirOp::ReadWrite | RedirOp::DupRead => out.push('0'),
            RedirOp::DupWrite => out.push('1'),
            _ => {}
        }
    }
    let close = matches!(&r.target, RedirTarget::Word(w) if w.as_literal() == Some("-"));
    let strip_tabs = matches!(&r.target, RedirTarget::HereDoc(doc) if doc.borrow().strip_tabs);
    out.push_str(match r.op {
        RedirOp::Read => "<",
        RedirOp::Write => ">",
        RedirOp::Clobber => ">|",
        RedirOp::Append => ">>",
        RedirOp::ReadWrite => "<>",
        RedirOp::DupRead if close => ">&",
        RedirOp::DupRead => "<&",
        RedirOp::DupWrite => ">&",
        RedirOp::HereDoc if strip_tabs => "<<-",
        RedirOp::HereDoc => "<<",
        RedirOp::HereString => "<<<",
        RedirOp::WriteBoth => "&>",
        RedirOp::AppendBoth => "&>>",
    });
    match &r.target {
        RedirTarget::Word(w) | RedirTarget::Expanded(w, _) => {
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
            // The delimiter as it was written: no line of the body is it.
            out.push_str(&doc.written);
            text.push_str(&doc.delimiter);
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

/// An expression of `for ((;;))`: an empty one as the 1 it stands for,
/// as bash shows it.
/// An expression of `for ((...))`: as written but for the blanks before
/// it, as bash shows it, or 1 when there is none.
fn arith_part(w: &Word, out: &mut String) {
    let mut text = String::new();
    raw_word(w, &mut text);
    out.push_str(if text.trim().is_empty() { "1" } else { text.trim_start() });
}

/// A subscript as it was written, near enough: as `declare -f` shows it,
/// and as bash names it in messages.
pub fn subscript_text(w: &Word) -> String {
    let mut out = String::new();
    subscript_word(w, &mut out);
    out
}

fn subscript_word(w: &Word, out: &mut String) {
    for part in &w.parts {
        match part {
            WordPart::Lit(s) => out.push_str(s),
            other => word_part(other, false, out),
        }
    }
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
                // In single quotes, whatever it holds, as bash shows it.
                out.push('\'');
                out.push_str(&s.replace('\'', "'\\''"));
                out.push('\'');
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
    if let ParamOp::Bad(text) = &p.op {
        out.push_str("${");
        out.push_str(text);
        out.push('}');
        return;
    }
    let name = match &p.name {
        ParamName::Var(name) => name.clone(),
        ParamName::Positional(n) => n.to_string(),
        ParamName::Special(c) => c.to_string(),
    };
    let simple = p.index.is_none() && matches!(p.op, ParamOp::None) && !p.indirect && !p.braced;
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
        subscript_word(index, out);
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
        ParamOp::BadTransform(text) => {
            out.push('@');
            out.push_str(text);
        }
        ParamOp::Names(all) => out.push(if *all { '@' } else { '*' }),
        ParamOp::Keys(all) => out.push_str(if *all { "[@]" } else { "[*]" }),
        ParamOp::Bad(_) => unreachable!("written above"),
    }
    out.push('}');
}
