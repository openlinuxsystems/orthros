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
        Err(Error::Syntax { messages, .. }) => messages.last().map(|&(line, _)| line),
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
fn coproc_names_only_a_compound_command() {
    let coproc = |text: &str| -> (String, String) {
        let program = parse(text, "test").unwrap_or_else(|e| panic!("{text:?}: {e:?}"));
        let Command::Coproc(coproc) = &program.commands[0].items[0].0.first.commands[0] else {
            panic!("{text:?} is no coprocess");
        };
        (coproc.name.clone(), show::job_line(&coproc.command))
    };
    assert_eq!(coproc("coproc cat -u"), ("COPROC".into(), "cat -u".into()));
    assert_eq!(coproc("coproc N { cat; }"), ("N".into(), "{ cat; }".into()));
    assert_eq!(coproc("coproc \"N\" (exit 2)"), ("N".into(), "( exit 2 )".into()));
    assert_eq!(coproc("coproc { cat; }"), ("COPROC".into(), "{ cat; }".into()));
    assert_eq!(coproc("coproc N cat"), ("COPROC".into(), "N cat".into()));
    assert_eq!(coproc("coproc x=1 env"), ("COPROC".into(), "x=1 env".into()));
    assert_eq!(coproc("coproc 1x [[ -n a ]]"), ("1x".into(), "[[ -n a ]]".into()));
    assert_eq!(coproc("coproc N time true").1, "N time true");
    round_trip("coproc cat 2> /dev/null");
    round_trip("coproc W while read -r l; do echo $l; done");
    for text in [
        "coproc",
        "coproc # comment",
        "coproc ! true",
        "coproc N then",
        "coproc N }",
        "coproc N function g { :; }",
        "coproc f() { :; }",
        "coproc a=b { :; }",
        "true | !",
    ] {
        assert_eq!(syntax_line(text), Some(1), "{text:?}");
    }
}

#[test]
fn bang_and_time_may_stand_alone() {
    for text in ["!", "! ;", "time", "time -p", "! !", "! time", "time !", "!\necho"] {
        assert!(parse(text, "test").is_ok(), "{text:?}");
    }
    // bash shows `time` first.
    assert_eq!(lines("! time true"), vec!["time ! true"]);
    assert_eq!(syntax_line("! && true"), Some(1));
}

#[test]
fn for_and_select_take_a_brace_body() {
    for text in ["for x in a b; { echo $x; }", "for x; { :; }", "select x in a; { break; }"] {
        assert!(parse(text, "test").is_ok(), "{text:?}");
    }
    let posix = Options { posix: true, ..Options::default() };
    let parsed = Parser::new("for x in a; { :; }", Rc::from("test"), 1, posix).parse();
    assert!(parsed.is_err());
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
        "[[ -f x",
        "[[ a &&",
        "[[ a =~ (x",
        "(a",
    ] {
        assert!(incomplete(text), "{text:?} should be incomplete");
    }
    // At the real end of the text a here-document ends there, with
    // bash's warning; while more may come, it goes on.
    let mut parser = Parser::new("cat <<EOF\nbody", Rc::from("test"), 1, options());
    assert!(parser.next_command().unwrap().is_some());
    let warning = "warning: here-document at line 1 delimited by end-of-file (wanted `EOF')";
    assert_eq!(parser.warnings(), vec![(2, warning.to_string())]);
    let mut parser = Parser::new("", Rc::from("test"), 1, options());
    parser.push_text("cat <<EOF\nbody");
    assert!(matches!(parser.next_command(), Err(Error::Incomplete(Unfinished::HereDoc(_)))));
}

/// bash's messages for `text`, each with its line.
fn messages(text: &str) -> Vec<(u32, String)> {
    match parse(text, "test") {
        Err(Error::Syntax { messages, .. }) => messages,
        Err(Error::Incomplete(unfinished)) => unfinished.report(99),
        Ok(_) => Vec::new(),
    }
}

fn texts(text: &str) -> Vec<String> {
    messages(text).into_iter().map(|(_, text)| text).collect()
}

#[test]
fn cond_commands_read_as_bash_reads_them() {
    // Newlines where a term starts and after a whole one.
    for text in ["[[\na ]]", "[[ a &&\n b ]]", "[[ ( a == b\n) ]]", "[[ ! \n a ]]", "[[ -f a\n]]"] {
        assert!(messages(text).is_empty(), "{text:?}: {:?}", messages(text));
    }
    // Not inside one: after a lone word, `[[ ( a\n== b ) ]]` is an error.
    assert_eq!(syntax_line("[[ ( a\n== b ) ]]"), Some(1));
    // A unary test takes any word, `==` too; one without its operand is a
    // word of its own only in `[` and `test`.
    round_trip("[[ -n == ]]");
    assert_eq!(lines("[[ -ef ]]"), ["[[ -n -ef ]]"]);
    round_trip("[[ a =~ x|y ]]");
    round_trip("[[ a =~ (a b) ]]");
    round_trip("[[ -e <(true) ]]");
    round_trip("[[ -n a && ( -n b || ! ( -n c ) ) ]]");
    // A lone word shows as bash shows it.
    assert_eq!(lines("[[ ( a ) ]]"), ["[[ ( -n a ) ]]"]);
}

#[test]
fn cond_errors_are_bash_s() {
    let near = |token: &str| format!("syntax error near `{token}'");
    assert_eq!(texts("[[ ]]"), [near("]]")]);
    assert_eq!(
        texts("[[ a\n]]"),
        ["unexpected token `newline', conditional binary operator expected".to_string(), near("a"),]
    );
    assert_eq!(texts("[[ a b ]]"), ["conditional binary operator expected".to_string(), near("b")]);
    assert_eq!(
        texts("[[ a == b c ]]"),
        ["syntax error in conditional expression".to_string(), near("c"),]
    );
    assert_eq!(
        texts("[[ && a ]]"),
        ["unexpected token `&&' in conditional command".to_string(), near("&"),]
    );
    assert_eq!(
        texts("[[ -f ]]"),
        ["unexpected argument `]]' to conditional unary operator".to_string(), near("]]"),]
    );
    assert_eq!(
        texts("[[ a < (b) ]]"),
        ["unexpected argument `(' to conditional binary operator".to_string(), near("(b"),]
    );
    assert_eq!(
        texts("[[ a ;; ]]"),
        ["unexpected token `;;', conditional binary operator expected".to_string(), near(";"),]
    );
    // The `)` messages are at the parentheses' lines.
    assert_eq!(
        messages("[[ (\n( a b"),
        [
            (2, "conditional binary operator expected".to_string()),
            (2, "expected `)'".to_string()),
            (1, "expected `)'".to_string()),
            (2, near("b")),
        ]
    );
    assert_eq!(
        messages("[[ ( a == b ]]"),
        [(1, "unexpected token `]]', expected `)'".to_string()), (1, near("]]")),]
    );
    // "In conditional expression" is at the `[[`'s line.
    assert_eq!(
        messages("[[ a == b\n\n) ]]"),
        [
            (1, "syntax error in conditional expression: unexpected token `)'".to_string()),
            (3, near(")")),
        ]
    );
    // At the end of the text.
    assert_eq!(
        messages("[[ a &&"),
        [
            (99, "unexpected token `EOF' in conditional command".to_string()),
            (99, "syntax error: unexpected end of file".to_string()),
        ]
    );
    assert_eq!(
        messages("[[ ( ( a == b"),
        [
            (1, "unexpected token `EOF', expected `)'".to_string()),
            (1, "expected `)'".to_string()),
            (99, "syntax error: unexpected end of file".to_string()),
        ]
    );
    assert_eq!(
        messages("\n[[ a == b"),
        [
            (2, "unexpected EOF while looking for `]]'".to_string()),
            (99, "syntax error: unexpected end of file".to_string()),
        ]
    );
    // Without a newline at the end, bash finds one there.
    assert_eq!(
        texts("[[ a =="),
        ["unexpected argument `newline' to conditional binary operator".to_string(), near("=="),]
    );
}

#[test]
fn only_a_separator_lets_a_command_follow_on_its_line() {
    for text in
        ["{ a; } b", "[[ a ]] b", "(a) b", "if a; then b; fi c", "f() { a; } b", "{ a; } >x b"]
    {
        assert_eq!(syntax_line(text), Some(1), "{text:?}");
    }
    assert_eq!(lines("{ a; }; b &"), ["{ a; }", "b"]);
}

#[test]
fn subscripts_at_a_command_s_start_run_to_their_bracket() {
    round_trip("a[1 + 1]=x m[a b]=1 m['c]d']=2 m[e\\]f]=3 m[\"$g h\"]=4 cmd");
    round_trip("m=([p q]=1 ['r]s']=2)");
    // Elsewhere a blank ends the word, as in bash.
    assert_eq!(lines("echo a[1 b]=c"), ["echo a[1 b]=c"]);
    assert_eq!(lines("declare a[1 b]=c"), ["declare a[1 b]=c"]);
    // A word that is no assignment is one word all the same.
    let program = parse("a[1 b] c", "test").unwrap();
    let Some(Command::Simple(command)) = program.commands[0].items[0].0.first.commands.first()
    else {
        panic!("not a simple command");
    };
    assert_eq!(command.words.len(), 2);
    assert!(incomplete("a[1 b"));
}

#[test]
fn a_command_s_line_is_where_bash_puts_it() {
    let line = |text: &str| {
        let program = parse(text, "test").unwrap();
        match program.commands[0].items[0].0.first.commands.first() {
            Some(Command::Simple(command)) => command.line,
            _ => panic!("not a simple command"),
        }
    };
    // Where the token after the first word ends, or where the first
    // assignment or redirection does.
    assert_eq!(line("a \\\n b \\\n c"), 2);
    assert_eq!(line("a \\\n\n"), 2);
    assert_eq!(line("\"a\nb\" c"), 2);
    assert_eq!(line("a \"b\nc\" d"), 2);
    assert_eq!(line("x=1 \\\n y=2 \\\n a"), 1);
    assert_eq!(line(">f \\\n a"), 1);
    assert_eq!(line("a <<E\nbody\nE\n"), 1);
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

#[test]
fn references_name_a_variable_or_an_element() {
    for good in ["a", "_x1", "a[1]", "a[x y]", "a[b[1]]", "a[']']", "a[\\]]"] {
        assert!(is_reference(good), "{good}");
    }
    for bad in ["", "1x", "a b", "a[", "a[]", "a[1", "a[1]x", "a[1][2]", "[1]"] {
        assert!(!is_reference(bad), "{bad}");
    }
}

#[test]
fn a_dash_after_a_duplication_stands_alone() {
    // bash: `>&-1` closes the output and passes 1.
    assert_eq!(lines("echo x >&-1"), vec!["echo x 1 1>&-"]);
    assert_eq!(lines("cat <&- q 2>& -y"), vec!["cat q y 0>&- 2>&-"]);
    assert_eq!(lines("echo >&7- >&$fd-"), vec!["echo 1>&7- 1>&$fd-"]);
}

#[test]
fn a_descriptor_variable_may_be_an_element() {
    assert_eq!(
        lines("exec {a[1]}>f {b[i+1]}<&- {c[$((1))]}>&2"),
        vec!["exec {a[1]}> f {b[i+1]}>&- {c[$((1))]}>&2"]
    );
    // Not variables: the words are the command's.
    for text in ["{a[}>f", "{a[]}>f", "{a[1]x}>f"] {
        let program = parse(text, "test").unwrap();
        let Command::Simple(simple) = &program.commands[0].items[0].0.first.commands[0] else {
            panic!("{text}")
        };
        assert!(!simple.words.is_empty(), "{text}");
    }
}

#[test]
fn braces_stay_as_written() {
    assert_eq!(lines("echo ${x}y $x ${1} \"${x}\""), vec!["echo ${x}y $x ${1} \"${x}\""]);
}
