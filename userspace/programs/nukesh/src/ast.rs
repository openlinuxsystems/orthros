// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The syntax tree the parser makes and the executor runs.
//!
//! It follows the grammar of POSIX.1-2024's Shell Command Language
//! (XCU 2.10) with the bash extensions Nukesh takes: `[[ ]]`, `(( ))`,
//! arithmetic `for`, `select`, `function`, `|&`, `&>`, here-strings,
//! process substitution, `$'...'` and brace expansion.

use std::cell::RefCell;
use std::rc::Rc;

/// A whole script, or one line typed at the prompt: complete commands in
/// order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub commands: Vec<CompleteCommand>,
}

/// And-or lists, each run in the foreground or the background.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompleteCommand {
    pub items: Vec<(AndOr, Sep)>,
}

/// How an and-or list ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sep {
    /// `;` or a newline: wait for it.
    Seq,
    /// `&`: run it in the background.
    Async,
}

/// Pipelines joined by `&&` and `||`.
#[derive(Clone, Debug, PartialEq)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(AndOrOp, Pipeline)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndOrOp {
    And,
    Or,
}

/// Commands joined by `|`, perhaps negated with `!` or timed with
/// `time`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pipeline {
    pub bang: bool,
    /// `time`, and with `-p` its POSIX output format.
    pub time: Option<bool>,
    pub commands: Vec<Command>,
    /// For each `|` after a command, whether it was `|&`, which joins
    /// standard error too.
    pub pipe_stderr: Vec<bool>,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Simple(SimpleCommand),
    Compound(Box<CompoundCommand>, Vec<Redirect>),
    /// `name() body` or `function name body`.
    Function(Rc<FunctionDef>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    pub body: Command,
    /// Where it was defined, for `declare -F` and error messages.
    pub file: Rc<str>,
    pub line: u32,
}

/// Assignments, words and redirections, in any order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimpleCommand {
    pub assigns: Vec<Assign>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    pub line: u32,
}

/// `name=value`, `name+=value`, `name[sub]=value` or `name=(a b c)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Assign {
    pub name: String,
    pub index: Option<Word>,
    pub append: bool,
    pub value: AssignValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AssignValue {
    Scalar(Word),
    /// `(a b [k]=v)`: each element, with its subscript if it has one.
    Array(Vec<(Option<Word>, Word)>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CompoundCommand {
    Brace(Program),
    Subshell(Program),
    For {
        var: String,
        /// `None` for `for x; do`, which loops over `"$@"`.
        words: Option<Vec<Word>>,
        body: Program,
        line: u32,
    },
    /// `for ((init; cond; step))`.
    ArithFor {
        init: Word,
        cond: Word,
        step: Word,
        body: Program,
        line: u32,
    },
    Select {
        var: String,
        words: Option<Vec<Word>>,
        body: Program,
        line: u32,
    },
    While {
        cond: Program,
        body: Program,
    },
    Until {
        cond: Program,
        body: Program,
    },
    If {
        /// `if` and each `elif`: a condition and its body.
        arms: Vec<(Program, Program)>,
        otherwise: Option<Program>,
    },
    Case {
        word: Word,
        arms: Vec<CaseArm>,
        line: u32,
    },
    /// `[[ expression ]]`.
    Cond(CondExpr, u32),
    /// `(( expression ))`.
    Arith(Word, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Program,
    pub end: CaseEnd,
}

/// How a case arm ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseEnd {
    /// `;;`: the case is done.
    Break,
    /// `;&`: run the next arm's body too.
    FallThrough,
    /// `;;&`: test the next arms' patterns too.
    Continue,
}

/// A `[[ ]]` expression.
#[derive(Clone, Debug, PartialEq)]
pub enum CondExpr {
    And(Box<CondExpr>, Box<CondExpr>),
    Or(Box<CondExpr>, Box<CondExpr>),
    Not(Box<CondExpr>),
    /// `-f file` and the other one-operand tests.
    Unary(String, Word),
    /// `a == b`, `a =~ re`, `a -lt b` and the other two-operand tests.
    Binary(Word, String, Word),
    /// A lone word: true if it is not empty.
    Word(Word),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Redirect {
    /// The descriptor it changes, if given (`2>`); `{var}>` names a
    /// variable to put a new descriptor in instead.
    pub fd: Option<RedirFd>,
    pub op: RedirOp,
    pub target: RedirTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RedirFd {
    Number(i32),
    Var(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirOp {
    /// `<`
    Read,
    /// `>`
    Write,
    /// `>|`
    Clobber,
    /// `>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `<&`
    DupRead,
    /// `>&`
    DupWrite,
    /// `<<` and `<<-`
    HereDoc,
    /// `<<<`
    HereString,
    /// `&>`: standard output and standard error to a file.
    WriteBoth,
    /// `&>>`
    AppendBoth,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RedirTarget {
    Word(Word),
    /// Filled in when the parser reaches the end of the line.
    HereDoc(Rc<RefCell<HereDoc>>),
}

/// A here-document's body, read after the line its `<<` is on.
#[derive(Clone, Debug, PartialEq)]
pub struct HereDoc {
    /// With an unquoted delimiter, the body is expanded as in double
    /// quotes; with a quoted one, it is taken as it is.
    pub body: Word,
    pub expand: bool,
}

/// A word, as its parts were written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WordPart {
    /// Unquoted text, where glob characters, tildes and braces mean
    /// something.
    Lit(String),
    /// A backslash-quoted character.
    Escaped(char),
    /// `'...'`, `$'...'` once decoded.
    Quoted(String),
    /// `"..."`.
    Double(Vec<WordPart>),
    /// `$name`, `${...}`.
    Param(Box<ParamExp>),
    /// `$(...)` or `` `...` ``.
    Command(Rc<Program>),
    /// `$((...))`: its text, expanded, then evaluated.
    Arith(Word),
    /// `<(...)` (false) or `>(...)` (true).
    ProcSub(bool, Rc<Program>),
    /// `name=(a b c)` as an argument of `declare` and the other
    /// declaration commands; it is the whole word.
    ArrayAssign(Box<Assign>),
}

/// A parameter expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamExp {
    pub name: ParamName,
    /// `[sub]` after the name.
    pub index: Option<Word>,
    pub op: ParamOp,
    /// `${!name}`: indirection.
    pub indirect: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamName {
    Var(String),
    /// `$1`, `${10}`.
    Positional(usize),
    /// `$@`, `$*`, `$#`, `$?`, `$-`, `$$`, `$!`, `$0`.
    Special(char),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamOp {
    None,
    /// `${#name}`.
    Length,
    /// `${name-word}` (false) and `${name:-word}` (true).
    Default(bool, Word),
    Assign(bool, Word),
    Error(bool, Word),
    Alternative(bool, Word),
    /// `${name#pat}` (false) and `${name##pat}` (true).
    RemovePrefix(bool, Word),
    RemoveSuffix(bool, Word),
    /// `${name:offset}` and `${name:offset:length}`.
    Substring(Word, Option<Word>),
    /// `${name/pat/rep}`: how it is anchored, the pattern and the
    /// replacement.
    Replace(ReplaceKind, Word, Option<Word>),
    /// `${name^pat}`, `${name^^pat}`, `${name,pat}`, `${name,,pat}`.
    Case(bool, bool, Option<Word>),
    /// `${name@Q}` and the other transformations.
    Transform(char),
    /// `${!prefix*}` (false) or `${!prefix@}` (true).
    Names(bool),
    /// `${!name[@]}` (true) or `${!name[*]}`: the array's keys.
    Keys(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceKind {
    First,
    All,
    Prefix,
    Suffix,
}

impl Word {
    /// A word of one unquoted literal.
    pub fn lit(text: &str) -> Word {
        Word { parts: vec![WordPart::Lit(text.to_string())] }
    }

    /// The word's text if it is all literal and unquoted.
    pub fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Lit(text)] => Some(text),
            _ => None,
        }
    }

    /// Whether any part of the word is quoted, which turns off keywords,
    /// aliases and, for here-document delimiters, expansion.
    pub fn has_quotes(&self) -> bool {
        self.parts.iter().any(|part| {
            matches!(part, WordPart::Quoted(_) | WordPart::Double(_) | WordPart::Escaped(_))
        })
    }

    /// The word with its quotes taken off and nothing expanded: how a
    /// here-document's delimiter is compared.
    pub fn unquoted_text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            unquoted_part(part, &mut out);
        }
        out
    }
}

fn unquoted_part(part: &WordPart, out: &mut String) {
    match part {
        WordPart::Lit(text) | WordPart::Quoted(text) => out.push_str(text),
        WordPart::Escaped(c) => out.push(*c),
        WordPart::Double(parts) => {
            for part in parts {
                unquoted_part(part, out);
            }
        }
        WordPart::Param(_)
        | WordPart::Command(_)
        | WordPart::Arith(_)
        | WordPart::ProcSub(..)
        | WordPart::ArrayAssign(_) => {}
    }
}
