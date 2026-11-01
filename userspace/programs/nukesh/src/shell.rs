// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The shell's state, and the variables seen through it: the special
//! parameters, the variables that change as you read them (`RANDOM`,
//! `SECONDS`, `LINENO`), and the attributes that change what assigning
//! does.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use indexmap::IndexMap;

use crate::arith::Index;
use crate::ast::{Command, FunctionDef, Sep};
use crate::jobs::Jobs;
use crate::vars::{Assoc, Value, Var, Vars, attr};
use crate::{arith, sys, text, trap, vars};

/// What stops a command from simply finishing with a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// `break n` and `continue n`, with the loops still to leave.
    Break(u32),
    Continue(u32),
    /// `return`, with its status.
    Return(i32),
    /// `exit`, or an error that ends a non-interactive shell.
    Exit(i32),
    /// An error that abandons the command line: a non-interactive shell
    /// exits with the status, an interactive one goes back to the prompt.
    Fatal(i32),
    /// An error that abandons the command being run from the script, the
    /// `.` file, the `eval` string or the trap, which then goes on with
    /// its next command, as in bash: an arithmetic error, a bad
    /// substitution, an assignment to a read-only variable. A subshell
    /// ends. In POSIX mode, these are `Fatal`, as in dash.
    Discard(i32),
    /// Ctrl-C at the prompt's command: back to the prompt.
    Interrupt,
}

/// A command's status, or why it did not finish.
pub type Res = Result<i32, Flow>;

/// `set -o` options.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub allexport: bool,
    pub notify: bool,
    pub noclobber: bool,
    pub errexit: bool,
    pub noglob: bool,
    pub hashall: bool,
    pub monitor: bool,
    pub noexec: bool,
    pub nounset: bool,
    pub verbose: bool,
    pub xtrace: bool,
    pub braceexpand: bool,
    pub errtrace: bool,
    pub functrace: bool,
    pub ignoreeof: bool,
    pub emacs: bool,
    pub vi: bool,
    pub pipefail: bool,
    pub posix: bool,
    pub physical: bool,
    pub privileged: bool,
    pub interactive_comments: bool,
    /// `set -k`: assignments after the command name go to its
    /// environment too.
    pub keyword: bool,
    pub restricted: bool,
    pub onecmd: bool,
    /// bash's `nolog`, which does nothing there either.
    pub nolog: bool,
}

/// The `set -o` options: long name, letter (or none), and where it is.
pub const OPTION_TABLE: &[(&str, Option<char>)] = &[
    ("allexport", Some('a')),
    ("braceexpand", Some('B')),
    ("emacs", None),
    ("errexit", Some('e')),
    ("errtrace", Some('E')),
    ("functrace", Some('T')),
    ("hashall", Some('h')),
    ("ignoreeof", None),
    ("interactive-comments", None),
    ("keyword", Some('k')),
    ("monitor", Some('m')),
    ("noclobber", Some('C')),
    ("noexec", Some('n')),
    ("noglob", Some('f')),
    ("nolog", None),
    ("notify", Some('b')),
    ("nounset", Some('u')),
    ("onecmd", Some('t')),
    ("physical", Some('P')),
    ("pipefail", None),
    ("posix", None),
    ("privileged", Some('p')),
    ("verbose", Some('v')),
    ("vi", None),
    ("xtrace", Some('x')),
];

impl Opts {
    pub fn get_mut(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "allexport" => &mut self.allexport,
            "braceexpand" => &mut self.braceexpand,
            "emacs" => &mut self.emacs,
            "errexit" => &mut self.errexit,
            "errtrace" => &mut self.errtrace,
            "functrace" => &mut self.functrace,
            "hashall" => &mut self.hashall,
            "ignoreeof" => &mut self.ignoreeof,
            "interactive-comments" => &mut self.interactive_comments,
            "keyword" => &mut self.keyword,
            "monitor" => &mut self.monitor,
            "noclobber" => &mut self.noclobber,
            "noexec" => &mut self.noexec,
            "noglob" => &mut self.noglob,
            "nolog" => &mut self.nolog,
            "notify" => &mut self.notify,
            "nounset" => &mut self.nounset,
            "onecmd" => &mut self.onecmd,
            "physical" => &mut self.physical,
            "pipefail" => &mut self.pipefail,
            "posix" => &mut self.posix,
            "privileged" => &mut self.privileged,
            "verbose" => &mut self.verbose,
            "vi" => &mut self.vi,
            "xtrace" => &mut self.xtrace,
            _ => return None,
        })
    }

    pub fn get(&self, name: &str) -> Option<bool> {
        self.clone().get_mut(name).map(|b| *b)
    }

    pub fn by_letter(letter: char) -> Option<&'static str> {
        OPTION_TABLE.iter().find(|(_, l)| *l == Some(letter)).map(|(name, _)| *name)
    }
}

/// What names an exported function in the environment.
const FUNCTION_PREFIX: &str = "BASH_FUNC_";

/// `shopt` options, and whether each is on by default, in bash's order.
/// Those nukesh has no use for (the `compat` levels, spelling correction,
/// `mailwarn`) are taken and do nothing, so that a `.bashrc` or a script
/// that sets them goes on; `restricted_shell` cannot be changed.
pub const SHOPTS: &[(&str, bool)] = &[
    ("autocd", false),
    ("assoc_expand_once", false),
    ("cdable_vars", false),
    ("cdspell", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("compat31", false),
    ("compat32", false),
    ("compat40", false),
    ("compat41", false),
    ("compat42", false),
    ("compat43", false),
    ("compat44", false),
    ("complete_fullquote", true),
    ("direxpand", false),
    ("dirspell", false),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", false),
    ("extdebug", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("gnu_errfmt", false),
    ("histappend", false),
    ("histreedit", false),
    ("histverify", false),
    ("hostcomplete", true),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("mailwarn", false),
    ("no_empty_cmd_completion", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
    ("restricted_shell", false),
    ("shift_verbose", false),
    ("sourcepath", true),
    ("varredir_close", false),
    ("xpg_echo", false),
];

/// One function call or sourced file, for `FUNCNAME`, `BASH_SOURCE`,
/// `BASH_LINENO` and `caller`.
#[derive(Clone, Debug)]
pub struct Frame {
    pub name: String,
    /// The file the function was defined in, or the file sourced.
    pub file: Rc<str>,
    /// The line it was called from.
    pub call_line: u32,
    /// The options as they were before `local -` in the function, put back
    /// when it returns.
    pub saved_opts: Option<Box<Opts>>,
}

pub struct Shell {
    pub vars: Vars,
    pub functions: HashMap<String, Rc<FunctionDef>>,
    pub aliases: IndexMap<String, String>,
    pub opts: Opts,
    pub shopts: BTreeMap<&'static str, bool>,
    /// `$1` onwards, and `$0`.
    pub args: Vec<String>,
    pub arg0: String,
    /// `$?`.
    pub status: i32,
    /// `$!`.
    pub last_bg: Option<i32>,
    /// `$$`: the shell's own process, which subshells keep.
    pub pid: i32,
    pub jobs: Jobs,
    /// Each trap's command: `None` for the default, `Some("")` to ignore.
    pub traps: Vec<Option<String>>,
    /// In a subshell, until it sets a trap of its own: the traps of the
    /// shell it was made from, which `trap` lists, as POSIX has it.
    pub parent_traps: Option<Vec<Option<String>>>,
    /// Signals ignored when a non-interactive shell started, which it
    /// cannot trap.
    pub ignored: Vec<bool>,
    pub interactive: bool,
    /// The letter `$-` ends with, as bash's: `c` for `-c`, `s` for
    /// commands read from standard input.
    pub source_flag: Option<char>,
    /// Running the text of `-c`.
    pub command_text: bool,
    pub login: bool,
    /// How many subshells deep this process is.
    pub subshell: u32,
    /// A command substitution's child, which reports no job a signal
    /// ended, as in bash.
    pub in_subst: bool,
    /// The line bash keeps for what is not a simple command's own news: a
    /// job a signal ended, a compound command's redirections. It is where
    /// the command read last ends, a function's body line while it runs,
    /// and the line of a `for`, `case`, `select`, `[[` or `((` run since.
    pub report_line: u32,
    /// Loops we are inside, for `break` and `continue`.
    pub loops: u32,
    pub frames: Vec<Frame>,
    /// The file being read, for error messages.
    pub source: Rc<str>,
    /// The line being run.
    pub lineno: u32,
    /// Commands found on `PATH`.
    pub hash: HashMap<String, String>,
    pub dirstack: Vec<String>,
    /// How deep we are in a context where `set -e` does not apply: the
    /// condition of `if`, `while` and `until`, the left of `&&` and `||`,
    /// and `!` pipelines.
    pub errexit_off: u32,
    /// The last pipeline failed where `set -e` was ignored (`a` in
    /// `a && b`), or was a compound command that failed so, or its failure
    /// was dealt with: a compound command that fails because of it does
    /// not run the ERR trap again.
    pub failed_ignored: bool,
    pub pipestatus: Vec<i32>,
    pub started: Instant,
    pub seconds_offset: i64,
    pub random: u32,
    /// `getopts`'s place inside a cluster of letters.
    pub getopts_char: usize,
    /// The command substitutions' last status, for an assignment-only
    /// command.
    pub subst_status: Option<i32>,
    /// The parsed text of `BASH_REMATCH`.
    pub rematch: Vec<String>,
    /// Programmable completion: each command's specification.
    pub completions: crate::complete::Specs,
    pub history: crate::history::History,
    /// colors.toml(5), read when the shell is interactive (ADR 0258).
    pub colors: Rc<crate::colors::Colors>,
    /// `--private`: no command record, folder counts or history file.
    pub private: bool,
    /// settings.toml(5), read when the shell is interactive.
    pub settings: Rc<crate::settings::Settings>,
    /// The command record every interactive shell shares (ADR 0262).
    pub records: Option<crate::record::Records>,
    /// How long the last command line took, for the prompt.
    pub last_millis: u64,
    /// The last command typed, for Alt+E on an empty line.
    pub last_command: String,
    /// What the folder file in use set (ADR 0276), and the files told
    /// about this session.
    pub project: Option<crate::project::Applied>,
    pub project_warned: std::collections::HashSet<String>,
    /// The folders `cd` left, for Alt+Left, and those Alt+Left left, for
    /// Alt+Right; and the folder at the last prompt (ADR 0262).
    pub dirs_back: Vec<String>,
    pub dirs_forward: Vec<String>,
    pub last_dir: String,
    /// Abbreviations, `abbr` (ADR 0262).
    pub abbrs: IndexMap<String, String>,
    /// The terminal the shell controls, with job control.
    pub tty: Option<i32>,
    pub shell_pgid: i32,
    pub tty_modes: Option<libc::termios>,
    /// Times `exit` was refused because of stopped jobs.
    pub exit_warned: bool,
    /// Commands run, for the prompt's `\#`.
    pub command_number: u64,
    /// The trap running now, which a nested signal does not rerun.
    pub in_trap: bool,
    /// The status from before the trap action running now, which `exit`
    /// with no number leaves, as POSIX has it: in the EXIT trap, and in
    /// POSIX mode in every trap. bash's other traps leave `$?`.
    pub trap_status: Option<i32>,
    /// The descriptors of `{name}>file` redirections.
    pub named_fds: Vec<i32>,
    /// The program name for messages: `nukesh` or `sh`.
    pub name: String,
    /// The descriptors that redirections saved, by slot: copies at 10 or
    /// above, out of the way of the script's own.
    pub held: Vec<Option<i32>>,
    /// `readonly -f` and `export -f`.
    pub readonly_functions: HashSet<String>,
    pub exported_functions: HashSet<String>,
    /// The pipes of process substitutions still open, closed after the
    /// command that used them.
    pub procsub_fds: Vec<i32>,
    /// Builtins turned off with `enable -n`.
    pub disabled: HashSet<String>,
    /// Set when text from `eval` or `.` had a syntax error.
    pub syntax_failed: bool,
    /// Set by an expansion error that ends the shell (`set -u`, `${x?}`),
    /// after which bash's `-c` exits with 127.
    pub expansion_failed: bool,
    /// How often each hashed command was used, for `hash`.
    pub hash_hits: HashMap<String, u32>,
    /// A completion function asked to be tried again (status 124).
    pub completions_retry: bool,
    /// Tab found nothing that starts with the word: file and command
    /// names match ignoring case, then with a typo (ADR 0273).
    pub completions_loose: bool,
    /// Running a script file, whose functions see `main` at the bottom of
    /// `FUNCNAME`.
    pub in_script: bool,
}

impl Shell {
    pub fn new(name: &str, posix: bool) -> Shell {
        let mut shopts = BTreeMap::new();
        for (option, on) in SHOPTS {
            shopts.insert(*option, *on);
        }
        // As `sh`, `echo` takes backslash escapes, as XSI and dash have it.
        shopts.insert("xpg_echo", posix);
        let opts = Opts {
            hashall: true,
            braceexpand: !posix,
            interactive_comments: true,
            posix,
            emacs: true,
            ..Opts::default()
        };
        let mut shell = Shell {
            vars: Vars::default(),
            functions: HashMap::new(),
            aliases: IndexMap::new(),
            opts,
            shopts,
            args: Vec::new(),
            arg0: name.to_string(),
            status: 0,
            last_bg: None,
            pid: sys::getpid(),
            jobs: Jobs::default(),
            traps: vec![None; trap::SLOTS],
            parent_traps: None,
            ignored: vec![false; trap::MAX_SIGNAL + 1],
            interactive: false,
            source_flag: None,
            command_text: false,
            login: false,
            subshell: 0,
            in_subst: false,
            report_line: 0,
            loops: 0,
            frames: Vec::new(),
            source: Rc::from(name),
            lineno: 0,
            hash: HashMap::new(),
            dirstack: Vec::new(),
            errexit_off: 0,
            failed_ignored: false,
            pipestatus: vec![0],
            started: Instant::now(),
            seconds_offset: 0,
            random: sys::random_u32(),
            getopts_char: 0,
            subst_status: None,
            rematch: Vec::new(),
            completions: crate::complete::Specs::default(),
            history: crate::history::History::default(),
            colors: Rc::default(),
            private: false,
            settings: Rc::default(),
            records: None,
            last_millis: 0,
            last_command: String::new(),
            project: None,
            project_warned: std::collections::HashSet::new(),
            dirs_back: Vec::new(),
            dirs_forward: Vec::new(),
            last_dir: String::new(),
            abbrs: IndexMap::new(),
            tty: None,
            shell_pgid: 0,
            tty_modes: None,
            exit_warned: false,
            command_number: 1,
            in_trap: false,
            trap_status: None,
            named_fds: Vec::new(),
            name: name.to_string(),
            held: Vec::new(),
            readonly_functions: HashSet::new(),
            exported_functions: HashSet::new(),
            procsub_fds: Vec::new(),
            disabled: HashSet::new(),
            syntax_failed: false,
            expansion_failed: false,
            hash_hits: HashMap::new(),
            completions_retry: false,
            completions_loose: false,
            in_script: false,
        };
        shell.import_environment();
        shell
    }

    /// Takes in the environment, and sets the variables the shell keeps.
    fn import_environment(&mut self) {
        for (key, value) in std::env::vars_os() {
            let name = text::from_os(&key);
            if let Some(function) =
                name.strip_prefix(FUNCTION_PREFIX).and_then(|n| n.strip_suffix("%%"))
            {
                self.import_function(function, &text::from_os(&value));
                continue;
            }
            if !crate::parse::is_name(&name) {
                continue;
            }
            let var = self.vars.entry(&name);
            var.value = Some(Value::Str(text::from_os(&value)));
            var.attrs |= attr::EXPORT;
        }
        let set_default = |shell: &mut Shell, name: &str, value: String| {
            if shell.vars.get(name).is_none() {
                shell.vars.entry(name).value = Some(Value::Str(value));
            }
        };
        set_default(self, "IFS", " \t\n".into());
        set_default(self, "PS1", if sys::geteuid() == 0 { "# " } else { "$ " }.into());
        set_default(self, "PS2", "> ".into());
        let ps4 = if self.opts.posix { "+ " } else { "+ ${BASH_SOURCE:-$0}:${LINENO}: " };
        set_default(self, "PS4", ps4.into());
        set_default(self, "OPTIND", "1".into());
        set_default(self, "PATH", "/system/commands:/bin:/usr/bin".into());
        let readonly = |shell: &mut Shell, name: &str, value: String| {
            let var = shell.vars.entry(name);
            var.value = Some(Value::Str(value));
            var.attrs |= attr::READONLY;
        };
        readonly(self, "PPID", sys::getppid().to_string());
        readonly(self, "UID", sys::getuid().to_string());
        readonly(self, "EUID", sys::geteuid().to_string());
        let version = env!("CARGO_PKG_VERSION");
        self.vars.entry("NUKESH_VERSION").value = Some(Value::Str(version.into()));
        if !self.opts.posix {
            // The version of bash whose language the shell runs, for the
            // scripts that check it before they use a feature, such as
            // `((BASH_VERSINFO[0] >= 4))` before `declare -A`, or that
            // refuse to run without bash (ADR 0103). NUKESH_VERSION tells
            // the shells apart.
            let machine = format!("{}-unknown-{}", std::env::consts::ARCH, std::env::consts::OS);
            let parts = ["5", "2", "0", "1", "release", &machine];
            self.vars.entry("BASH_VERSION").value = Some(Value::Str("5.2.0(1)-release".into()));
            let var = self.vars.entry("BASH_VERSINFO");
            let map = (0..).zip(parts.iter().map(ToString::to_string)).collect();
            var.value = Some(Value::Array(map));
            var.attrs |= attr::READONLY | attr::ARRAY;
        }
        if self.vars.get("HOSTNAME").is_none() {
            self.vars.entry("HOSTNAME").value = Some(Value::Str(sys::hostname()));
        }
        // SHLVL counts nested shells.
        let level = self.get("SHLVL").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
        let var = self.vars.entry("SHLVL");
        var.value = Some(Value::Str(level.max(1).to_string()));
        var.attrs |= attr::EXPORT;
        // PWD is the logical directory, if it names where we are.
        let real = std::env::current_dir().map(|p| text::from_os(p.as_os_str())).ok();
        let pwd = self.get("PWD").filter(|pwd| {
            pwd.starts_with('/')
                && match (std::fs::metadata(text::to_os(pwd)), std::fs::metadata(".")) {
                    (Ok(a), Ok(b)) => {
                        use std::os::unix::fs::MetadataExt;
                        a.dev() == b.dev() && a.ino() == b.ino()
                    }
                    _ => false,
                }
        });
        if let Some(pwd) = pwd.or(real) {
            let var = self.vars.entry("PWD");
            var.value = Some(Value::Str(pwd));
            var.attrs |= attr::EXPORT;
        }
    }

    pub fn shopt(&self, name: &str) -> bool {
        self.shopts.get(name).copied().unwrap_or(false)
    }

    pub fn posix(&self) -> bool {
        self.opts.posix
    }

    /// Prints an error, naming the shell and, when reading a script, the
    /// line.
    pub fn error(&self, message: &str) {
        let line = if self.interactive && self.frames.is_empty() {
            format!("{}: {message}\n", self.name)
        } else {
            format!("{}: line {}: {message}\n", self.source, self.lineno)
        };
        let _ = sys::write_all(2, &text::encode(&line));
    }

    /// Prints an error as bash prints a failed system call's: naming the
    /// shell, without a line.
    pub fn error_unlined(&self, message: &str) {
        let who =
            if self.interactive && self.frames.is_empty() { &*self.name } else { &*self.source };
        let _ = sys::write_all(2, &text::encode(&format!("{who}: {message}\n")));
    }

    /// A syntax error, as bash prints it: after `label`, what the text
    /// came from (`eval`, `-c`, `exit trap`), when it has one; at its line,
    /// or for text that ended, at the line bash gives, from `end` (see
    /// [`crate::parse::Parser::end_line`]); then, but in an interactive
    /// shell, the line it is on.
    pub fn syntax_error(&mut self, error: &crate::parse::Error, label: Option<&str>, end: u32) {
        let (messages, context) = match error {
            crate::parse::Error::Incomplete(unfinished) => (unfinished.report(end), None),
            crate::parse::Error::Syntax { messages, context } => {
                (messages.clone(), context.clone())
            }
        };
        let last = messages.last().map_or(end, |&(line, _)| line);
        let context = context.filter(|_| !self.interactive).map(|text| (last, format!("`{text}'")));
        for (line, text) in messages.into_iter().chain(context) {
            self.lineno = line;
            match label {
                Some(label) => {
                    let who = if self.interactive && self.frames.is_empty() {
                        &*self.name
                    } else {
                        &*self.source
                    };
                    let text = format!("{who}: {label}: line {line}: {text}\n");
                    let _ = sys::write_all(2, &text::encode(&text));
                }
                None => self.error(&text),
            }
        }
        self.lineno = last;
    }

    /// Prints the parser's warnings, each at its line.
    pub fn parse_warnings(&mut self, parser: &mut crate::parse::Parser) {
        for (line, warning) in parser.warnings() {
            self.lineno = line;
            self.error(&warning);
        }
    }

    /// A fatal error: printed, and the command line abandoned.
    pub fn fatal(&mut self, message: &str, status: i32) -> Flow {
        self.error(message);
        Flow::Fatal(status)
    }

    /// [`Shell::fatal`] for an expansion that cannot go on, as bash's
    /// FORCE_EOF.
    pub fn expansion_fatal(&mut self, message: &str, status: i32) -> Flow {
        self.expansion_failed = true;
        self.fatal(message, status)
    }

    /// An error that abandons the command being run, after which a script
    /// goes on with the next ([`Flow::Discard`]); fatal in POSIX mode,
    /// with dash's status.
    pub fn discard(&mut self, message: &str) -> Flow {
        self.discard_with(message, 1)
    }

    /// [`Shell::discard`], with the status the shell then has.
    pub fn discard_with(&mut self, message: &str, status: i32) -> Flow {
        self.error(message);
        if self.posix() { Flow::Fatal(2) } else { Flow::Discard(status) }
    }

    /// Whether aliases are expanded: with `expand_aliases`, on in an
    /// interactive shell, and as POSIX has it in every POSIX one, as in
    /// bash.
    pub fn expands_aliases(&self) -> bool {
        self.posix() || self.shopt("expand_aliases")
    }

    /// The parser's options for the shell's current settings.
    pub fn parse_options(&self) -> crate::parse::Options {
        let aliases = self.expands_aliases() && !self.aliases.is_empty();
        crate::parse::Options {
            posix: self.posix(),
            extglob: self.shopt("extglob"),
            aliases: aliases.then(|| {
                Rc::new(self.aliases.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }),
        }
    }

    // Variables.

    /// Follows `declare -n` references to the variable they name.
    pub fn resolve(&self, name: &str) -> String {
        let mut name = name.to_string();
        for _ in 0..16 {
            match self.vars.get(&name) {
                Some(var) if var.has(attr::NAMEREF) => match &var.value {
                    Some(Value::Str(target)) if !target.is_empty() => {
                        // A reference to an element, `ref=a[2]`, stops here.
                        if !crate::parse::is_name(target) {
                            return target.clone();
                        }
                        name.clone_from(target);
                    }
                    _ => return name,
                },
                _ => return name,
            }
        }
        name
    }

    /// A variable's value as a string: `None` if unset.
    pub fn get(&self, name: &str) -> Option<String> {
        if let Some(value) = self.dynamic(name) {
            return value.scalar().map(str::to_string);
        }
        let name = self.resolve(name);
        if let Some((base, index)) = split_element(&name) {
            return self.get_element(base, index);
        }
        self.vars.get(&name)?.value.as_ref()?.scalar().map(str::to_string)
    }

    /// A variable's whole value: `None` if unset.
    pub fn get_value(&self, name: &str) -> Option<Value> {
        if let Some(value) = self.dynamic(name) {
            return Some(value);
        }
        let name = self.resolve(name);
        self.vars.get(&name)?.value.clone()
    }

    /// Whether `name` is set; for an array, whether it has an element.
    pub fn is_set(&self, name: &str) -> bool {
        self.get_value(name).is_some_and(|v| v.len() > 0)
    }

    /// The variables whose value comes from the shell's state.
    fn dynamic(&self, name: &str) -> Option<Value> {
        // In POSIX mode, which takes no `[[`, bash's own are not set, as
        // in dash: scripts take `BASH_SUBSHELL` for a sign of bash.
        if self.opts.posix && name.starts_with("BASH") {
            return None;
        }
        let s = |text: String| Some(Value::Str(text));
        let array = |items: Vec<String>| {
            Some(Value::Array(items.into_iter().enumerate().map(|(i, v)| (i as i64, v)).collect()))
        };
        match name {
            "LINENO" => s(self.lineno.to_string()),
            "SECONDS" => {
                s((self.started.elapsed().as_secs() as i64 + self.seconds_offset).to_string())
            }
            "EPOCHSECONDS" => s(sys::now().0.to_string()),
            "EPOCHREALTIME" => {
                let (secs, micros) = sys::now();
                s(format!("{secs}.{micros:06}"))
            }
            "BASHPID" => s(sys::getpid().to_string()),
            "BASH_SUBSHELL" => s(self.subshell.to_string()),
            "FUNCNAME" => {
                let names: Vec<String> = self
                    .frames
                    .iter()
                    .rev()
                    .filter(|f| f.name != "source")
                    .map(|f| f.name.clone())
                    .chain((self.in_script && !self.frames.is_empty()).then(|| "main".to_string()))
                    .collect();
                if names.is_empty() { None } else { array(names) }
            }
            "BASH_SOURCE" => {
                let mut files: Vec<String> =
                    self.frames.iter().rev().map(|f| f.file.to_string()).collect();
                if self.frames.is_empty() || !self.interactive {
                    files.push(self.script_name());
                }
                array(files)
            }
            "BASH_LINENO" => {
                let mut lines: Vec<String> =
                    self.frames.iter().rev().map(|f| f.call_line.to_string()).collect();
                lines.push("0".into());
                array(lines)
            }
            "PIPESTATUS" => array(self.pipestatus.iter().map(ToString::to_string).collect()),
            "BASH_REMATCH" if !self.rematch.is_empty() => array(self.rematch.clone()),
            "BASH_ARGV0" => s(self.arg0.clone()),
            "HISTCMD" => s(self.history.count().to_string()),
            _ => None,
        }
    }

    /// The name of the script the shell runs, for `BASH_SOURCE`.
    fn script_name(&self) -> String {
        if self.interactive { String::new() } else { self.arg0.clone() }
    }

    /// The next `RANDOM`: 0 to 32767, as bash's generator makes them.
    pub fn next_random(&mut self) -> u32 {
        // A 32-bit linear congruential generator, its top bits.
        self.random = self.random.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (self.random >> 16) & 0x7fff
    }

    fn get_element(&self, name: &str, index: &str) -> Option<String> {
        if self.assoc(name) {
            return match self.get_value(name)? {
                Value::Assoc(map) => map.get(index).cloned(),
                _ => None,
            };
        }
        let index = self.eval_index_readonly(index)?;
        let value = self.get_value(name);
        let Some(at) = vars::position(value.as_ref(), index) else {
            self.error(&format!("{name}: bad array subscript"));
            return None;
        };
        match value? {
            Value::Str(s) => (at == 0).then_some(s),
            Value::Array(map) => map.get(&at).cloned(),
            Value::Assoc(_) => None,
        }
    }

    /// An index of an indexed array, evaluated without side effects being
    /// kept (for reads through `get`).
    fn eval_index_readonly(&self, index: &str) -> Option<i64> {
        if let Some(n) = arith::parse_number(index) {
            return Some(n);
        }
        let mut copy = ReadOnlyEnv(self);
        arith::eval(index, &mut copy).ok()
    }

    /// Whether `name` is an associative array, set or only declared.
    pub fn assoc(&self, name: &str) -> bool {
        self.vars
            .get(&self.resolve(name))
            .is_some_and(|v| v.has(attr::ASSOC) || matches!(v.value, Some(Value::Assoc(_))))
    }

    /// Evaluates arithmetic in the shell's variables, for an expansion.
    pub fn arith(&mut self, text: &str) -> Result<i64, Flow> {
        match arith::eval(text, self) {
            Ok(value) => Ok(value),
            Err(message) => Err(self.discard(&message)),
        }
    }

    /// Evaluates arithmetic for a command (`((...))`, `let`, `for ((`,
    /// `[[`'s integers), which fails on an error instead, as in bash;
    /// the error is printed after `what`.
    pub fn arith_command(&mut self, what: &str, text: &str) -> Option<i64> {
        match arith::eval(text, self) {
            Ok(value) => Some(value),
            Err(message) => {
                self.error(&format!("{what}: {message}"));
                None
            }
        }
    }

    /// Checks that `name` may be assigned.
    pub fn writable(&self, name: &str) -> Result<(), String> {
        if self.vars.get(name).is_some_and(|v| v.has(attr::READONLY)) {
            return Err(format!("{name}: readonly variable"));
        }
        if self.opts.restricted
            && matches!(name, "PATH" | "SHELL" | "ENV" | "BASH_ENV" | "HISTFILE")
        {
            return Err(format!("{name}: restricted"));
        }
        Ok(())
    }

    /// Changes `value` as the variable's attributes say: arithmetic for
    /// `-i`, case for `-l` and `-u`.
    fn transform(
        &mut self,
        name: &str,
        value: String,
        old: Option<&str>,
    ) -> Result<String, String> {
        let attrs = self.vars.get(name).map_or(0, |v| v.attrs);
        let mut value = value;
        if attrs & attr::INTEGER != 0 {
            let text = match old {
                Some(old) => format!("{} + ({value})", if old.is_empty() { "0" } else { old }),
                None => value,
            };
            value = arith::eval(&text, self)?.to_string();
        } else if let Some(old) = old {
            value = format!("{old}{value}");
        }
        if attrs & attr::LOWER != 0 {
            value = value.to_lowercase();
        } else if attrs & attr::UPPER != 0 {
            value = value.to_uppercase();
        }
        Ok(value)
    }

    /// `name=value` before a command, in the scope pushed for the
    /// command's assignments: exported, as bash makes it. A read-only
    /// variable is left as it is, with bash's error, and the command still
    /// runs; `+=` appends through the variable's attributes, which a plain
    /// `=` here does not take, as in bash.
    pub fn assign_temp(&mut self, name: &str, value: String, append: bool) {
        let value = match self.writable(name) {
            Err(message) => Err(message),
            Ok(()) if append => {
                let old = self.get(name).unwrap_or_default();
                self.transform(name, value, Some(&old))
            }
            Ok(()) => Ok(value),
        };
        match value {
            Ok(value) => self.vars.set_temp(name, temp_var(value, true)),
            Err(message) => self.error(&message),
        }
    }

    /// `name=value`, or with `append`, `name+=value`.
    pub fn assign(&mut self, name: &str, value: String, append: bool) -> Result<(), String> {
        let name = self.resolve(name);
        if let Some((base, index)) = split_element(&name) {
            let (base, index) = (base.to_string(), index.to_string());
            return self.assign_element(&base, &index, value, append);
        }
        self.writable(&name)?;
        // `x+=text` on a plain string grows it where it is, so that a loop
        // building a string takes time in proportion to it, not to its
        // square.
        let plain = attr::INTEGER | attr::LOWER | attr::UPPER;
        if append
            && !matches!(name.as_str(), "PATH" | "RANDOM" | "SECONDS" | "OPTIND")
            && let Some(var) = self.vars.get_mut(&name)
            && var.attrs & plain == 0
            && let Some(Value::Str(current)) = &mut var.value
        {
            current.push_str(&value);
            if self.opts.allexport {
                var.attrs |= attr::EXPORT;
            }
            return Ok(());
        }
        let current = self.vars.get(&name).and_then(|v| v.value.as_ref());
        let old = if append {
            Some(current.and_then(Value::scalar).unwrap_or("").to_string())
        } else {
            None
        };
        let is_array = matches!(current, Some(Value::Array(_) | Value::Assoc(_)));
        let value = self.transform(&name, value, old.as_deref())?;
        self.special_assigned(&name, &value);
        if is_array {
            return self.assign_element(&name, "0", value, false);
        }
        let allexport = self.opts.allexport;
        let var = self.vars.entry(&name);
        var.value = Some(Value::Str(value));
        if allexport {
            var.attrs |= attr::EXPORT;
        }
        Ok(())
    }

    /// Sets `name`, a name or an element, for the builtin `builtin`
    /// (`printf -v`, `read`), as bash does: false, with the error, if the
    /// variable refuses the value (read-only, an element before the start
    /// of the array). An arithmetic error abandons the command: one in the
    /// subscript as an assignment's does, one in the value of an integer
    /// variable named for the builtin.
    pub fn assign_from(&mut self, builtin: &str, name: &str, value: String) -> Result<bool, Flow> {
        let name = self.expand_element(name)?;
        let name = self.resolve(&name);
        let result = match split_element(&name) {
            Some((base, index)) if !self.assoc(base) => {
                let (base, index) = (self.resolve(base), index.to_string());
                match self.writable(&base) {
                    Err(message) => Err(message),
                    Ok(()) => {
                        let at = arith::eval(&index, self).map_err(|m| self.discard(&m))?;
                        match self.assign_at(&base, at, value, false) {
                            Ok(true) => Ok(()),
                            Ok(false) => Err(format!("{base}[{index}]: bad array subscript")),
                            Err(message) => Err(message),
                        }
                    }
                }
            }
            _ => self.assign(&name, value, false),
        };
        self.builtin_assigned(builtin, result)
    }

    /// [`Shell::assign_from`] for a whole indexed array (`read -a`).
    pub fn assign_array_from(
        &mut self,
        builtin: &str,
        name: &str,
        elements: Vec<(Option<String>, String)>,
    ) -> Result<bool, Flow> {
        let result = self.assign_array(name, elements, false);
        self.builtin_assigned(builtin, result)
    }

    fn builtin_assigned(
        &mut self,
        builtin: &str,
        result: Result<(), String>,
    ) -> Result<bool, Flow> {
        match result {
            Ok(()) => Ok(true),
            Err(message) if refusal(&message) => {
                self.error(&message);
                Ok(false)
            }
            Err(message) => Err(self.discard(&format!("{builtin}: {message}"))),
        }
    }

    /// `name[index]=value`.
    pub fn assign_element(
        &mut self,
        name: &str,
        index: &str,
        value: String,
        append: bool,
    ) -> Result<(), String> {
        let name = self.resolve(name);
        self.writable(&name)?;
        if self.assoc(&name) {
            let old = append.then(|| self.get_element(&name, index).unwrap_or_default());
            let value = self.transform(&name, value, old.as_deref())?;
            let var = self.vars.entry(&name);
            if !matches!(var.value, Some(Value::Assoc(_))) {
                var.value = Some(Value::Assoc(Assoc::new()));
            }
            if let Some(Value::Assoc(map)) = &mut var.value {
                map.insert(index.to_string(), value);
            }
            return Ok(());
        }
        let at = arith::eval(index, self)?;
        if self.assign_at(&name, at, value, append)? {
            Ok(())
        } else {
            Err(format!("{name}[{index}]: bad array subscript"))
        }
    }

    /// Element `at` of the indexed array `name` (resolved and writable),
    /// which a string becomes, as element 0: `false`, assigning nothing,
    /// when a negative `at` reaches before the start.
    fn assign_at(
        &mut self,
        name: &str,
        at: i64,
        value: String,
        append: bool,
    ) -> Result<bool, String> {
        let var = self.vars.entry(name);
        let map = match var.value.take() {
            Some(Value::Array(map)) => map,
            Some(Value::Str(s)) => BTreeMap::from([(0, s)]),
            Some(Value::Assoc(_)) | None => BTreeMap::new(),
        };
        let at = vars::array_position(&map, at);
        let old = at.filter(|_| append).map(|at| map.get(&at).cloned().unwrap_or_default());
        self.vars.entry(name).value = Some(Value::Array(map));
        let Some(at) = at else { return Ok(false) };
        let value = self.transform(name, value, old.as_deref())?;
        if let Some(Value::Array(map)) = &mut self.vars.entry(name).value {
            map.insert(at, value);
        }
        Ok(true)
    }

    /// `name=(elements)`, or with `append`, `name+=(elements)`.
    pub fn assign_array(
        &mut self,
        name: &str,
        elements: Vec<(Option<String>, String)>,
        append: bool,
    ) -> Result<(), String> {
        let name = self.resolve(name);
        self.writable(&name)?;
        let assoc = self
            .vars
            .get(&name)
            .is_some_and(|v| v.has(attr::ASSOC) || matches!(v.value, Some(Value::Assoc(_))));
        if assoc {
            let mut map = match (append, self.vars.get(&name).and_then(|v| v.value.clone())) {
                (true, Some(Value::Assoc(map))) => map,
                // Emptied, the table keeps its size, as bash's does.
                (false, Some(Value::Assoc(mut map))) => {
                    map.clear();
                    map
                }
                _ => Assoc::new(),
            };
            let mut pending_key: Option<String> = None;
            for (key, value) in elements {
                match key {
                    Some(key) => {
                        map.insert(key, self.transform(&name, value, None)?);
                    }
                    // `(k1 v1 k2 v2)`: keys and values in turn.
                    None => match pending_key.take() {
                        Some(key) => {
                            map.insert(key, self.transform(&name, value, None)?);
                        }
                        None => pending_key = Some(value),
                    },
                }
            }
            if let Some(key) = pending_key {
                map.insert(key, self.transform(&name, String::new(), None)?);
            }
            let var = self.vars.entry(&name);
            var.value = Some(Value::Assoc(map));
            return Ok(());
        }
        let mut map = match (append, self.vars.get(&name).and_then(|v| v.value.clone())) {
            (true, Some(Value::Array(map))) => map,
            (true, Some(Value::Str(s))) => BTreeMap::from([(0, s)]),
            _ => BTreeMap::new(),
        };
        let mut next = map.keys().next_back().map_or(0, |last| last + 1);
        for (key, value) in elements {
            if let Some(key) = key {
                // As bash, an index before the start is left out, warned of.
                let at = arith::eval(&key, self)?;
                match vars::array_position(&map, at) {
                    Some(at) => next = at,
                    None => {
                        self.error(&format!("[{key}]={value}: bad array subscript"));
                        continue;
                    }
                }
            }
            // The attributes apply to each element, as in bash.
            map.insert(next, self.transform(&name, value, None)?);
            next += 1;
        }
        let allexport = self.opts.allexport;
        let var = self.vars.entry(&name);
        var.value = Some(Value::Array(map));
        if allexport {
            var.attrs |= attr::EXPORT;
        }
        Ok(())
    }

    /// What assigning some variables does besides.
    fn special_assigned(&mut self, name: &str, value: &str) {
        match name {
            "PATH" => self.hash.clear(),
            "RANDOM" => self.random = value.parse().unwrap_or(0),
            "SECONDS" => {
                self.seconds_offset =
                    value.parse::<i64>().unwrap_or(0) - self.started.elapsed().as_secs() as i64;
            }
            "OPTIND" => self.getopts_char = 0,
            "LINENO" => {}
            _ => {}
        }
    }

    /// Sets a variable the shell owns, ignoring read-only and attributes.
    pub fn set_var(&mut self, name: &str, value: &str) {
        let var = self.vars.entry(name);
        var.value = Some(Value::Str(value.to_string()));
    }

    /// Unsets a variable the shell sets itself (OPTARG, COMPREPLY): false,
    /// with bash's error, if it is read-only.
    pub fn unset(&mut self, name: &str) -> bool {
        if self.vars.get(name).is_some_and(|v| v.has(attr::READONLY)) {
            self.error(&format!("{name}: readonly variable"));
            return false;
        }
        // Only a subscript has arithmetic to fail and abandon a command.
        self.unset_checked(name).unwrap_or(false)
    }

    /// `unset name` or `unset 'name[index]'`, as the builtin does: false,
    /// with an error printed, if it cannot be; an error in the subscript's
    /// arithmetic abandons the command, as in bash.
    pub fn unset_checked(&mut self, name: &str) -> Result<bool, Flow> {
        let name = self.expand_element(name)?;
        let name = self.resolve(&name);
        if let Some((base, index)) = split_element(&name) {
            let (base, index) = (base.to_string(), index.to_string());
            return self.unset_element(&base, &index);
        }
        Ok(self.unset_whole(&name))
    }

    fn unset_whole(&mut self, name: &str) -> bool {
        if self.vars.is_local(name) && !self.shopt("localvar_unset") {
            // An unset local stays local, hiding the global.
            let var = self.vars.local(name, false);
            if var.has(attr::READONLY) {
                self.error(&format!("unset: {name}: cannot unset: readonly variable"));
                return false;
            }
            var.value = None;
            var.attrs &= attr::EXPORT;
            return true;
        }
        if !self.vars.unset(name) {
            self.error(&format!("unset: {name}: cannot unset: readonly variable"));
            return false;
        }
        if name == "PATH" {
            self.hash.clear();
        }
        true
    }

    /// `unset 'name[index]'`: as bash, a string is element 0 alone, and a
    /// negative index counts back from an array's end.
    fn unset_element(&mut self, name: &str, index: &str) -> Result<bool, Flow> {
        if self.vars.get(name).is_some_and(|v| v.has(attr::READONLY)) {
            self.error(&format!("unset: {name}: cannot unset: readonly variable"));
            return Ok(false);
        }
        if index == "@" || index == "*" {
            return Ok(self.vars.unset(name));
        }
        if self.assoc(name) {
            if let Some(Value::Assoc(map)) = self.vars.get_mut(name).and_then(|v| v.value.as_mut())
            {
                map.remove(index);
            }
            return Ok(true);
        }
        let at = self.arith(index)?;
        let Some(var) = self.vars.get_mut(name) else { return Ok(true) };
        let declared_array = var.has(attr::ARRAY);
        let bad = match &mut var.value {
            Some(Value::Array(map)) => match vars::array_position(map, at) {
                Some(at) => {
                    map.remove(&at);
                    None
                }
                None => Some(format!("unset: [{index}]: bad array subscript")),
            },
            Some(Value::Str(_)) if at == 0 => {
                var.value = None;
                None
            }
            Some(Value::Str(_)) => Some(format!("unset: {name}: not an array variable")),
            None if at < 0 && declared_array => {
                Some(format!("unset: [{index}]: bad array subscript"))
            }
            _ => None,
        };
        match bad {
            Some(message) => {
                self.error(&message);
                Ok(false)
            }
            None => Ok(true),
        }
    }

    /// The environment for a program the shell runs, with the functions of
    /// `export -f` as bash passes them: `BASH_FUNC_name%%=() { body }`.
    pub fn environment(&self) -> Vec<String> {
        let mut environment = self.vars.environment();
        let mut names: Vec<&String> = self.exported_functions.iter().collect();
        names.sort();
        for name in names {
            if let Some(def) = self.functions.get(name) {
                // `name () \n{ ... }` as `() { ... }`.
                let text = crate::show::function(def);
                let rest = text.strip_prefix(name.as_str()).unwrap_or(&text).trim_start();
                let body = rest.strip_prefix("()").unwrap_or(rest).trim();
                environment.push(format!("{FUNCTION_PREFIX}{name}%%=() {body}"));
            }
        }
        environment
    }

    /// Defines a function passed in the environment by `export -f`, if its
    /// text is a function's and nothing more.
    fn import_function(&mut self, name: &str, body: &str) {
        if !body.starts_with("() {") || self.posix() {
            return;
        }
        let text = format!("{name} {body}");
        let Ok(program) = crate::parse::parse(&text, "environment") else { return };
        // One definition and nothing after it: text that goes on to a
        // command is not run.
        let [command] = program.commands.as_slice() else { return };
        let [(list, Sep::Seq)] = command.items.as_slice() else { return };
        let [Command::Function(def)] = list.first.commands.as_slice() else { return };
        if list.rest.is_empty() && !list.first.bang && def.name == name {
            self.functions.insert(name.to_string(), def.clone());
            self.exported_functions.insert(name.to_string());
        }
    }

    /// `set -m` or `set +m`: job control taken up or given up.
    pub fn monitor_changed(&mut self) {
        if self.opts.monitor && self.tty.is_none() {
            if self.interactive {
                crate::interactive::take_terminal_control(self);
            } else {
                crate::interactive::share_terminal(self);
            }
        }
        self.install_signals();
    }

    /// `$-`.
    pub fn flags(&self) -> String {
        // bash's order.
        let mut flags: String = "abefhikmnprtuvxBCEHPT"
            .chars()
            .filter(|&letter| match letter {
                'H' => self.history.expand,
                'i' => self.interactive,
                'r' => self.opts.restricted,
                _ => OPTION_TABLE
                    .iter()
                    .any(|(name, l)| *l == Some(letter) && self.opts.get(name) == Some(true)),
            })
            .collect();
        flags.extend(self.source_flag);
        flags
    }

    /// Whether a trap is set for slot `slot` (a signal or pseudo-signal).
    pub fn trap_command(&self, slot: usize) -> Option<&str> {
        self.traps.get(slot).and_then(|t| t.as_deref()).filter(|t| !t.is_empty())
    }
}

/// `a[i]`, as a name a reference can hold: the array and the subscript.
fn split_element(name: &str) -> Option<(&str, &str)> {
    let open = name.find('[')?;
    let inner = name[open + 1..].strip_suffix(']')?;
    Some((&name[..open], inner))
}

/// Whether an assignment's error only refused it (a read-only or
/// restricted variable, an element before the start of the array), and
/// was not arithmetic's, which abandons the command, as in bash.
fn refusal(message: &str) -> bool {
    [": readonly variable", ": restricted", ": bad array subscript"]
        .iter()
        .any(|end| message.ends_with(end))
}

/// Arithmetic through the shell's variables: assignments change them.
impl arith::Env for Shell {
    fn get(&mut self, name: &str, index: Option<Index>) -> Result<Option<String>, String> {
        let value = match index {
            Some(index) => self.get_element(name, index.key),
            None => Shell::get(self, name),
        };
        if value.is_none() && self.opts.nounset {
            return Err(format!("{name}: unbound variable"));
        }
        Ok(value)
    }

    fn set(&mut self, name: &str, index: Option<Index>, value: i64) -> Result<(), String> {
        let Some(index) = index else { return self.assign(name, value.to_string(), false) };
        if self.assoc(name) {
            return self.assign_element(name, index.key, value.to_string(), false);
        }
        let real = self.resolve(name);
        self.writable(&real)?;
        let at = arith::eval(index.key, self)?;
        // In arithmetic, as in bash, an index before the start is warned
        // of and the expression goes on.
        if !self.assign_at(&real, at, value.to_string(), false)? {
            self.error(&format!("{name}[{}]: bad array subscript", index.text));
        }
        Ok(())
    }

    /// An indexed array's subscript as its number, evaluated here, once.
    fn key(&mut self, name: &str, text: &str) -> Result<String, String> {
        if self.assoc(name) {
            return Ok(text.to_string());
        }
        Ok(arith::eval(text, self)?.to_string())
    }
}

/// Arithmetic that may read variables but changes none: for subscripts
/// read by [`Shell::get`].
struct ReadOnlyEnv<'a>(&'a Shell);

impl arith::Env for ReadOnlyEnv<'_> {
    fn get(&mut self, name: &str, index: Option<Index>) -> Result<Option<String>, String> {
        Ok(match index {
            Some(index) => self.0.get_element(name, index.key),
            None => self.0.get(name),
        })
    }

    fn set(&mut self, _: &str, _: Option<Index>, _: i64) -> Result<(), String> {
        Ok(())
    }
}

/// A new variable with a value, for [`Vars::set_temp`].
pub fn temp_var(value: String, exported: bool) -> Var {
    Var { value: Some(Value::Str(value)), attrs: if exported { attr::EXPORT } else { 0 } }
}
