// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Fuzzing: the parser, arithmetic, patterns and brace expansion take
//! random text, made of pieces of shell so that most of it gets past the
//! first token, and must answer or fail for all of it: never panic, loop
//! or overflow the stack. What the parser reads must also print, as
//! `type` and `declare -f` print commands, as text that reads back the
//! same. Text nested past the parser's limit must be an error.
//!
//! The seed is fixed, so a failure repeats; `NUKESH_FUZZ_SEED` and
//! `NUKESH_FUZZ_ROUNDS` change it and how many inputs each test tries.

use std::collections::HashMap;
use std::thread;

use crate::arith::{self, Env};
use crate::ast::{Command, Program, Word, WordPart};
use crate::{brace, parse, pattern, show};

/// A small, fast random number generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Rng {
        let seed = std::env::var("NUKESH_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
        Rng(seed.unwrap_or(0x9e37_79b9_7f4a_7c15) ^ salt | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Text of up to `most` pieces from `pieces`, with a random character
    /// now and then.
    fn text(&mut self, pieces: &[&str], most: usize) -> String {
        let mut out = String::new();
        for _ in 0..=self.below(most) {
            if self.below(16) == 0 {
                out.push(char::from(32 + self.below(95) as u8));
            } else {
                out.push_str(pieces[self.below(pieces.len())]);
            }
        }
        out
    }
}

/// How many inputs each test tries.
fn rounds() -> usize {
    std::env::var("NUKESH_FUZZ_ROUNDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000)
}

/// The stack a program's first thread has on Radix.
const STACK: usize = 8 << 20;

/// Runs `test` on a thread with `stack` bytes of stack.
fn with_stack(stack: usize, test: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .stack_size(stack)
        .spawn(test)
        .expect("a thread for the test")
        .join()
        .expect("the test passed");
}

/// Pieces of shell syntax.
const SYNTAX: &[&str] = &[
    "echo",
    " ",
    " ",
    " ",
    "x",
    "y1",
    "$x",
    "${x}",
    "${x:-a b}",
    "${x#*/}",
    "${x%%.*}",
    "${x//a/b}",
    "${#x}",
    "${x[1]}",
    "${x[@]}",
    "${!x}",
    "${x:1:2}",
    "${x@Q}",
    "\"",
    "\"",
    "'",
    "\\",
    "$(",
    ")",
    "`",
    "(",
    "{",
    "}",
    ";",
    ";;",
    ";&",
    "&",
    "&&",
    "||",
    "|",
    "|&",
    "\n",
    "\n",
    "<",
    ">",
    ">>",
    "2>&1",
    "&>",
    "<&-",
    "<<<",
    "<<EOF\n",
    "<<-X\n",
    "EOF\n",
    "\tX\n",
    "<(",
    ">(",
    "if",
    "then",
    "elif",
    "else",
    "fi",
    "for",
    "in",
    "do",
    "done",
    "while",
    "until",
    "case",
    "esac",
    "function",
    "f()",
    "[[",
    "]]",
    "((",
    "))",
    "$((",
    "$[",
    "1+2",
    "=",
    "a=1",
    "a+=2",
    "arr=(1 2)",
    "a[1]=x",
    "*",
    "?",
    "[a-z]",
    "[!x]",
    "@(",
    "!(",
    "+(",
    "#",
    "~",
    "~/",
    "$'\\n'",
    "$\"x\"",
    "!",
    "-n",
    "-f",
    "==",
    "!=",
    "=~",
    "<",
    "{a,b}",
    "{1..3}",
    "{a..c..2}",
    "\t",
    "local",
    "declare -a",
    "time",
    "coproc",
    "select",
    "export",
    "readonly",
    "$@",
    "$*",
    "$#",
    "$?",
    "$$",
    "$!",
    "$0",
    "$1",
    "${10}",
    "!!",
    "\\\n",
    "#comment\n",
];

/// Pieces of arithmetic.
const ARITH: &[&str] = &[
    "0",
    "1",
    "-1",
    "7",
    "08",
    "0x1f",
    "0X",
    "2#101",
    "64#@_",
    "36#zz",
    "1#1",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775808",
    "x",
    "y",
    "a[1]",
    "a[x]",
    "+",
    "-",
    "*",
    "/",
    "%",
    "**",
    "<<",
    ">>",
    "&",
    "|",
    "^",
    "~",
    "!",
    "&&",
    "||",
    "<",
    "<=",
    ">",
    ">=",
    "==",
    "!=",
    "?",
    ":",
    "=",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "<<=",
    ">>=",
    "&=",
    "|=",
    "^=",
    "++",
    "--",
    "(",
    ")",
    ",",
    " ",
    "$x",
    "${x}",
    "64",
    "-64",
    "63",
];

/// Pieces of patterns, and of the text they are matched against.
const PATTERNS: &[&str] = &[
    "a",
    "b",
    ".",
    "/",
    "*",
    "**",
    "?",
    "[",
    "]",
    "[a-c]",
    "[!a]",
    "[^b]",
    "[]]",
    "[[:alpha:]]",
    "[[:digit:]",
    "[=a=]",
    "[.a.]",
    "\\",
    "\\*",
    "@(",
    "?(",
    "*(",
    "+(",
    "!(",
    "|",
    ")",
    "{",
    "}",
    "-",
    "é",
    "日",
];

/// Arithmetic's variables, which the expressions may change.
#[derive(Default)]
struct Vars(HashMap<String, String>);

impl Env for Vars {
    fn get(&mut self, name: &str, index: Option<&str>) -> Result<Option<String>, String> {
        let key = match index {
            Some(i) => format!("{name}[{}]", arith::eval(i, self)?),
            None => name.to_string(),
        };
        Ok(self.0.get(&key).cloned())
    }

    fn set(&mut self, name: &str, index: Option<&str>, value: i64) -> Result<(), String> {
        let key = match index {
            Some(i) => format!("{name}[{}]", arith::eval(i, self)?),
            None => name.to_string(),
        };
        self.0.insert(key, value.to_string());
        Ok(())
    }
}

/// The function a program starts by defining, if it does.
fn first_function(program: &Program) -> Option<&Command> {
    let command = program.commands.first()?.items.first()?.0.first.commands.first()?;
    matches!(command, Command::Function(_)).then_some(command)
}

/// A function as `declare -f` prints it.
fn printed(function: &Command) -> String {
    match function {
        Command::Function(def) => show::function(def),
        _ => unreachable!(),
    }
}

#[test]
fn parser_takes_anything() {
    with_stack(STACK, || {
        let mut rng = Rng::new(1);
        for _ in 0..rounds() {
            // As the body of a function, which `declare -f` prints whole,
            // with its here-documents.
            let text = format!("f() {{\n{}\n}}", rng.text(SYNTAX, 24));
            let Ok(program) = parse::parse(&text, "fuzz") else { continue };
            // Each command prints on one line, as `jobs` shows it.
            let _ = show::program_line(&program);
            let Some(function) = first_function(&program) else { continue };
            // The function prints, and the print reads back as itself.
            let shown = printed(function);
            let again = parse::parse(&shown, "fuzz")
                .unwrap_or_else(|e| panic!("{text:?} printed as {shown:?}, which fails: {e:?}"));
            let function = first_function(&again).expect("a function");
            assert_eq!(printed(function), shown, "{text:?}");
        }
    });
}

#[test]
fn arithmetic_takes_anything() {
    with_stack(STACK, || {
        let mut rng = Rng::new(2);
        for _ in 0..rounds() {
            let text = rng.text(ARITH, 16);
            let mut vars = Vars::default();
            vars.0.insert("x".into(), "3".into());
            // Variables that name expressions, one of them itself.
            vars.0.insert("y".into(), "x*2".into());
            vars.0.insert("z".into(), "z+1".into());
            let _ = arith::eval(&text, &mut vars);
        }
    });
}

#[test]
fn patterns_take_anything() {
    with_stack(STACK, || {
        let mut rng = Rng::new(3);
        for _ in 0..rounds() {
            let text = rng.text(PATTERNS, 12);
            let subject = rng.text(PATTERNS, 12);
            let options = pattern::Options {
                extglob: rng.below(2) == 0,
                nocase: rng.below(4) == 0,
                file_name: rng.below(4) == 0,
                dotglob: rng.below(2) == 0,
            };
            let pattern = pattern::Pattern::new(&text, options);
            let _ = pattern.matches(&subject);
            let _ = pattern.match_prefix(&subject, rng.below(2) == 0);
            let _ = pattern.match_suffix(&subject, rng.below(2) == 0);
            let chars: Vec<char> = subject.chars().collect();
            let _ = pattern.find(&chars, 0, rng.below(2) == 0);
        }
    });
}

#[test]
fn brace_expansion_takes_anything() {
    with_stack(STACK, || {
        let mut rng = Rng::new(4);
        let pieces = &["a", "b", "{", "}", ",", "..", "1", "-3", "z", "\\", "{,}", "{a,b}"];
        for _ in 0..rounds() {
            let word = Word { parts: vec![WordPart::Lit(rng.text(pieces, 12))] };
            let _ = brace::expand(&word);
        }
    });
}

/// Text nested far past [`parse::MAX_DEPTH`] is an error, however it
/// nests; at the limit it is read. The host tests' debug build takes
/// about ten times the stack the release build on Radix does (which
/// reads, runs and prints text at the limit in under 2 MiB), so this runs
/// with eight times a program's stack.
#[test]
fn deep_nesting_is_an_error() {
    with_stack(8 * STACK, || {
        let deep = 20_000;
        let cases: [(&str, &str); 7] = [
            ("( ", " )"),
            ("{ ", "; }"),
            ("$(", ")"),
            ("echo \"$(", ")\""),
            ("${x:-", "}"),
            ("[[ ( ", " ) ]]"),
            ("if true; then ", "; fi"),
        ];
        for (open, close) in cases {
            let text = format!("{}x{}", open.repeat(deep), close.repeat(deep));
            assert!(parse::parse(&text, "fuzz").is_err(), "{open:?} nested {deep} deep");
        }
        assert!(parse::parse(&format!("[[ {}x ]]", "! ".repeat(deep)), "fuzz").is_err());
        let depth = parse::MAX_DEPTH as usize - 2;
        let text = format!("{}:{}", "( ".repeat(depth), " )".repeat(depth));
        let program = parse::parse(&text, "fuzz").expect("text at the limit");
        // Printing and dropping it go as deep.
        assert!(show::program_line(&program).len() > depth);
        drop(program);
        // Patterns and arithmetic nest too.
        let pattern = pattern::Pattern::new(
            &format!("{}a{}", "@(".repeat(deep), ")".repeat(deep)),
            pattern::Options { extglob: true, ..Default::default() },
        );
        let _ = pattern.matches("a");
        // Opened and never closed, which each `@(` finds once.
        let pattern = pattern::Pattern::new(
            &"@(a|".repeat(deep),
            pattern::Options { extglob: true, ..Default::default() },
        );
        assert!(!pattern.matches("a"));
        let mut vars = Vars::default();
        assert!(
            arith::eval(&format!("{}1{}", "(".repeat(deep), ")".repeat(deep)), &mut vars).is_err()
        );
        assert!(arith::eval(&"-".repeat(deep), &mut vars).is_err());
        let word = Word {
            parts: vec![WordPart::Lit(format!("{}a{}", "{a,".repeat(deep), "}".repeat(deep)))],
        };
        let _ = brace::expand(&word);
    });
}
