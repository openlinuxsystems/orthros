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

use crate::ast::{Command, FunctionDef, Sep};
use crate::jobs::Jobs;
use crate::vars::{Value, Var, Vars, attr};
use crate::{arith, sys, text, trap};

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
    pub restricted: bool,
    pub onecmd: bool,
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
    ("monitor", Some('m')),
    ("noclobber", Some('C')),
    ("noexec", Some('n')),
    ("noglob", Some('f')),
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
            "monitor" => &mut self.monitor,
            "noclobber" => &mut self.noclobber,
            "noexec" => &mut self.noexec,
            "noglob" => &mut self.noglob,
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

/// `shopt` options, and whether each is on by default.
pub const SHOPTS: &[(&str, bool)] = &[
    ("assoc_expand_once", false),
    ("autocd", false),
    ("cdable_vars", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("histappend", true),
    ("histreedit", false),
    ("histverify", false),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
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
    pub login: bool,
    /// How many subshells deep this process is.
    pub subshell: u32,
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
    /// `a && b`), or was a compound command that failed so.
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
    /// How often each hashed command was used, for `hash`.
    pub hash_hits: HashMap<String, u32>,
    /// A completion function asked to be tried again (status 124).
    pub completions_retry: bool,
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
            login: false,
            subshell: 0,
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
            tty: None,
            shell_pgid: 0,
            tty_modes: None,
            exit_warned: false,
            command_number: 1,
            in_trap: false,
            named_fds: Vec::new(),
            name: name.to_string(),
            held: Vec::new(),
            readonly_functions: HashSet::new(),
            exported_functions: HashSet::new(),
            procsub_fds: Vec::new(),
            disabled: HashSet::new(),
            syntax_failed: false,
            hash_hits: HashMap::new(),
            completions_retry: false,
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
        set_default(self, "PATH", "/system/bin:/bin:/usr/bin".into());
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
            let map = (0..).zip(parts.iter().map(|part| part.to_string())).collect();
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

    /// A fatal error: printed, and the command line abandoned.
    pub fn fatal(&mut self, message: &str, status: i32) -> Flow {
        self.error(message);
        Flow::Fatal(status)
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

    /// The parser's options for the shell's current settings. Aliases
    /// are expanded in an interactive shell and, as POSIX has it, in every
    /// POSIX one; elsewhere after `shopt -s expand_aliases`, as in bash.
    pub fn parse_options(&self) -> crate::parse::Options {
        let aliases = (self.interactive || self.posix() || self.shopt("expand_aliases"))
            && !self.aliases.is_empty();
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
                        name = target.clone();
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
            "PIPESTATUS" => array(self.pipestatus.iter().map(|s| s.to_string()).collect()),
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
        let value = self.get_value(name)?;
        match &value {
            Value::Assoc(map) => map.get(index).cloned(),
            _ => {
                let index = self.eval_index_readonly(index)?;
                match value {
                    Value::Str(s) => (index == 0 || index == -1).then_some(s),
                    Value::Array(map) => {
                        let index = if index < 0 {
                            map.keys().next_back().map_or(index, |last| last + 1 + index)
                        } else {
                            index
                        };
                        map.get(&index).cloned()
                    }
                    Value::Assoc(_) => None,
                }
            }
        }
    }

    /// An index of an indexed array, evaluated without side effects being
    /// kept (for reads through `get`).
    fn eval_index_readonly(&self, index: &str) -> Option<i64> {
        if let Ok(n) = index.trim().parse() {
            return Some(n);
        }
        let mut copy = ReadOnlyEnv(self);
        arith::eval(index, &mut copy).ok()
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
    fn writable(&self, name: &str) -> Result<(), String> {
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

    /// `name=value`, or with `append`, `name+=value`.
    pub fn assign(&mut self, name: &str, value: String, append: bool) -> Result<(), String> {
        let name = self.resolve(name);
        if let Some((base, index)) = split_element(&name) {
            let (base, index) = (base.to_string(), index.to_string());
            return self.assign_element(&base, &index, value, append);
        }
        self.writable(&name)?;
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
        let assoc = self
            .vars
            .get(&name)
            .is_some_and(|v| v.has(attr::ASSOC) || matches!(v.value, Some(Value::Assoc(_))));
        if assoc {
            let old = append.then(|| self.get_element(&name, index).unwrap_or_default());
            let value = self.transform(&name, value, old.as_deref())?;
            let var = self.vars.entry(&name);
            if !matches!(var.value, Some(Value::Assoc(_))) {
                var.value = Some(Value::Assoc(IndexMap::new()));
            }
            if let Some(Value::Assoc(map)) = &mut var.value {
                map.insert(index.to_string(), value);
            }
            return Ok(());
        }
        let mut at = arith::eval(index, self)?;
        let var = self.vars.entry(&name);
        let map = match var.value.take() {
            Some(Value::Array(map)) => map,
            Some(Value::Str(s)) => BTreeMap::from([(0, s)]),
            Some(Value::Assoc(_)) | None => BTreeMap::new(),
        };
        if at < 0 {
            match map.keys().next_back() {
                Some(last) if last + 1 + at >= 0 => at += last + 1,
                _ => {
                    self.vars.entry(&name).value = Some(Value::Array(map));
                    return Err(format!("{name}[{index}]: bad array subscript"));
                }
            }
        }
        let old = append.then(|| map.get(&at).cloned().unwrap_or_default());
        self.vars.entry(&name).value = Some(Value::Array(map));
        let value = self.transform(&name, value, old.as_deref())?;
        if let Some(Value::Array(map)) = &mut self.vars.entry(&name).value {
            map.insert(at, value);
        }
        Ok(())
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
                _ => IndexMap::new(),
            };
            let mut pending_key: Option<String> = None;
            for (key, value) in elements {
                match key {
                    Some(key) => {
                        map.insert(key, value);
                    }
                    // `(k1 v1 k2 v2)`: keys and values in turn.
                    None => match pending_key.take() {
                        Some(key) => {
                            map.insert(key, value);
                        }
                        None => pending_key = Some(value),
                    },
                }
            }
            if let Some(key) = pending_key {
                map.insert(key, String::new());
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
        let integer = self.vars.get(&name).is_some_and(|v| v.has(attr::INTEGER));
        for (key, value) in elements {
            if let Some(key) = key {
                next = arith::eval(&key, self)?;
                if next < 0 {
                    next += map.keys().next_back().map_or(0, |last| last + 1);
                }
            }
            let value = if integer { arith::eval(&value, self)?.to_string() } else { value };
            map.insert(next, value);
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
                    value.parse::<i64>().unwrap_or(0) - self.started.elapsed().as_secs() as i64
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

    /// `unset name`: false (with an error printed) if it is read-only.
    pub fn unset(&mut self, name: &str) -> bool {
        let name = self.resolve(name);
        if let Some((base, index)) = split_element(&name) {
            let (base, index) = (base.to_string(), index.to_string());
            return self.unset_element(&base, &index);
        }
        if self.vars.is_local(&name) && !self.shopt("localvar_unset") {
            // An unset local stays local, hiding the global.
            let var = self.vars.local(&name, false);
            if var.has(attr::READONLY) {
                self.error(&format!("{name}: cannot unset: readonly variable"));
                return false;
            }
            var.value = None;
            var.attrs &= attr::EXPORT;
            return true;
        }
        if !self.vars.unset(&name) {
            self.error(&format!("{name}: cannot unset: readonly variable"));
            return false;
        }
        if name == "PATH" {
            self.hash.clear();
        }
        true
    }

    fn unset_element(&mut self, name: &str, index: &str) -> bool {
        if self.vars.get(name).is_some_and(|v| v.has(attr::READONLY)) {
            self.error(&format!("{name}: cannot unset: readonly variable"));
            return false;
        }
        if index == "@" || index == "*" {
            return self.vars.unset(name);
        }
        let at = match self.vars.get(name).and_then(|v| v.value.as_ref()) {
            Some(Value::Assoc(_)) => None,
            _ => arith::eval(index, self).ok(),
        };
        let Some(var) = self.vars.get_mut(name) else { return true };
        match &mut var.value {
            Some(Value::Assoc(map)) => {
                map.shift_remove(index);
            }
            Some(Value::Array(map)) => {
                if let Some(mut at) = at {
                    if at < 0 {
                        at += map.keys().next_back().map_or(0, |last| last + 1);
                    }
                    map.remove(&at);
                }
            }
            Some(Value::Str(_)) if at == Some(0) => var.value = None,
            _ => {}
        }
        true
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
        let mut flags = String::new();
        for (name, letter) in OPTION_TABLE {
            if let Some(letter) = letter
                && self.opts.get(name) == Some(true)
            {
                flags.push(*letter);
            }
        }
        if self.history.expand {
            flags.push('H');
        }
        if self.interactive {
            flags.push('i');
        }
        if self.opts.restricted {
            flags.push('r');
        }
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

/// Arithmetic through the shell's variables: assignments change them.
impl arith::Env for Shell {
    fn get(&mut self, name: &str, index: Option<&str>) -> Result<Option<String>, String> {
        let value = match index {
            Some(index) => self.get_element(name, index),
            None => Shell::get(self, name),
        };
        if value.is_none() && self.opts.nounset {
            return Err(format!("{name}: unbound variable"));
        }
        Ok(value)
    }

    fn set(&mut self, name: &str, index: Option<&str>, value: i64) -> Result<(), String> {
        match index {
            Some(index) => self.assign_element(name, index, value.to_string(), false),
            None => self.assign(name, value.to_string(), false),
        }
    }
}

/// Arithmetic that may read variables but changes none: for subscripts
/// read by [`Shell::get`].
struct ReadOnlyEnv<'a>(&'a Shell);

impl arith::Env for ReadOnlyEnv<'_> {
    fn get(&mut self, name: &str, index: Option<&str>) -> Result<Option<String>, String> {
        Ok(match index {
            Some(index) => self.0.get_element(name, index),
            None => self.0.get(name),
        })
    }

    fn set(&mut self, _: &str, _: Option<&str>, _: i64) -> Result<(), String> {
        Ok(())
    }
}

/// A new variable with a value, for [`Vars::set_temp`].
pub fn temp_var(value: String, exported: bool) -> Var {
    Var { value: Some(Value::Str(value)), attrs: if exported { attr::EXPORT } else { 0 } }
}
