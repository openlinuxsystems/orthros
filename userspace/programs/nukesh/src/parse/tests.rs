// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::show;

fn options() -> Options {
    Options::default()
}

/// The text's commands, each as `show` prints an and-or list.
fn lines(text: &str) -> Vec<String> {
    let program = parse(text, "test").unwrap_or_else(|e| panic!("{text:?}: {e:?}"));
    program
        .commands
        .iter()
        .flat_map(|c| c.items.iter().map(|(list, _)| show::and_or_line(list)))
        .collect()
}

fn round_trip(text: &str) {
    assert_eq!(lines(text), vec![text.to_string()], "{text:?}");
}

fn incomplete(text: &str) -> bool {
    matches!(parse(text, "test"), Err(Error::Incomplete(_)))
}

fn syntax_line(text: &str) -> Option<u32> {
    match parse(text, "test") {
        Err(Error::Syntax { line, .. }) => Some(line),
        _ => None,
    }
}

#[test]
fn simple_commands_read_back_as_written() {
    round_trip("echo a b c");
    round_trip("a=1 b=2 env");
    round_trip("ls -l > out 2>&1");
    round_trip("cat < in >> out");
    round_trip("a | b | c");
    round_trip("a && b || c");
    round_trip("! grep -q x file");
}

#[test]
fn quoting_is_kept() {
    round_trip("echo 'a b' \"c $d\" e\\ f");
    round_trip("echo \"${x:-default}\" ${#y} ${z//a/b}");
    round_trip("echo $(date) $((1 + 2))");
}

#[test]
fn compound_commands_parse() {
    for text in [
        "if a; then b; elif c; then d; else e; fi",
        "while read x; do echo $x; done < file",
        "until false; do break; done",
        "for i in 1 2 3; do echo $i; done",
        "for ((i = 0; i < 3; i++)); do :; done",
        "case $x in a|b) echo ab ;; *) echo other ;; esac",
        "{ a; b; } > out",
        "(cd /tmp && ls)",
        "[[ -f file && $x == y* ]]",
        "(( x += 2 ))",
        "select x in a b; do break; done",
        "time sleep 1",
        "f() { echo hi; }",
        "function g { :; }",
    ] {
        assert!(parse(text, "test").is_ok(), "{text}");
    }
}

#[test]
fn several_commands_on_a_line() {
    assert_eq!(lines("a; b & c"), vec!["a", "b", "c"]);
    assert_eq!(lines("a\nb\n\nc\n").len(), 3);
}

#[test]
fn unfinished_text_is_incomplete() {
    for text in [
        "if true",
        "if true; then echo",
        "while :; do",
        "echo 'abc",
        "echo \"abc",
        "echo $(date",
        "echo ${x",
        "echo `date",
        "a |",
        "a &&",
        "f() {",
        "case x in",
        "cat <<EOF\nbody",
        "[[ -f x",
        "(a",
    ] {
        assert!(incomplete(text), "{text:?} should be incomplete");
    }
}

#[test]
fn mistakes_are_syntax_errors_with_their_line() {
    assert_eq!(syntax_line("echo )"), Some(1));
    assert_eq!(syntax_line("echo a\necho b\nfi"), Some(3));
    assert_eq!(syntax_line("; a"), Some(1));
    assert_eq!(syntax_line("| a"), Some(1));
    assert_eq!(syntax_line("if then fi"), Some(1));
    assert_eq!(syntax_line("case x in a) ;; esac )"), Some(1));
}

#[test]
fn here_documents_take_their_bodies() {
    let program = parse("cat <<EOF\nline $x\nEOF\necho after\n", "test").unwrap();
    assert_eq!(program.commands.len(), 2);
    assert!(parse("cat <<-EOF\n\tindented\n\tEOF\n", "test").is_ok());
    assert!(parse("cat <<'EOF'\n$not expanded\nEOF\n", "test").is_ok());
    // Two here-documents on one line, read in order.
    assert!(parse("cat <<A <<B\na\nA\nb\nB\n", "test").is_ok());
}

#[test]
fn commands_come_one_at_a_time() {
    let mut parser = Parser::new("echo a\nif true\n", Rc::from("test"), 1, options());
    assert!(parser.next_command().unwrap().is_some());
    // The `if` is not finished: the parser waits for more text.
    assert!(matches!(parser.next_command(), Err(Error::Incomplete(_))));
    parser.push_text("then echo b; fi\n");
    assert!(parser.next_command().unwrap().is_some());
    assert!(parser.next_command().unwrap().is_none());
}

#[test]
fn process_substitution_is_a_word_outside_posix_mode() {
    assert!(parse("diff <(sort a) <(sort b)", "test").is_ok());
    assert!(parse("tee >(wc -l) < file", "test").is_ok());
    let posix = Options { posix: true, ..Options::default() };
    assert!(Parser::new("diff <(a)", Rc::from("test"), 1, posix).parse().is_err());
}

#[test]
fn patterns_in_double_brackets_are_extended() {
    // As in bash, extglob is on for the right of `==` in `[[ ]]`.
    assert!(parse("[[ x == @(a|b) ]]", "test").is_ok());
    assert!(parse("[[ x != !(a) ]]", "test").is_ok());
}

#[test]
fn aliases_are_expanded() {
    let aliases: HashMap<String, String> = [("ll".to_string(), "ls -l".to_string())].into();
    let options = Options { aliases: Some(Rc::new(aliases)), ..Options::default() };
    let program = Parser::new("ll /tmp", Rc::from("test"), 1, options).parse().unwrap();
    let Command::Simple(command) = &program.commands[0].items[0].0.first.commands[0] else {
        panic!("not a simple command");
    };
    let words: Vec<_> = command.words.iter().filter_map(Word::as_literal).collect();
    assert_eq!(words, ["ls", "-l", "/tmp"]);
}

#[test]
fn reserved_words_are_words_elsewhere() {
    round_trip("echo if then fi");
    assert!(parse("echo done", "test").is_ok());
}

#[test]
fn word_lists_parse() {
    let words = super::words("a 'b c' $d", options()).unwrap();
    assert_eq!(words.len(), 3);
}
