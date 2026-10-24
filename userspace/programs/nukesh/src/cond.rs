// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Conditional expressions: the `test` and `[` builtins (XCU test), and
//! `[[ ... ]]`, which shares their tests and adds pattern matching, `=~`
//! regular expressions (through the C library's POSIX regcomp, as bash
//! does) and `&&`, `||` and `( )` inside it.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

use crate::ast::CondExpr;
use crate::pattern::Pattern;
use crate::shell::{Flow, Res, Shell};
use crate::{sys, text};

/// A unary test: `-f file`, `-z string` and the rest.
pub fn unary(shell: &Shell, op: &str, arg: &str) -> Option<bool> {
    let meta = || std::fs::metadata(text::to_os(arg)).ok();
    let link = || std::fs::symlink_metadata(text::to_os(arg)).ok();
    Some(match op {
        "-a" | "-e" => meta().is_some(),
        "-b" => meta().is_some_and(|m| m.file_type().is_block_device()),
        "-c" => meta().is_some_and(|m| m.file_type().is_char_device()),
        "-d" => meta().is_some_and(|m| m.is_dir()),
        "-f" => meta().is_some_and(|m| m.is_file()),
        "-g" => meta().is_some_and(|m| m.permissions().mode() & 0o2000 != 0),
        "-h" | "-L" => link().is_some_and(|m| m.file_type().is_symlink()),
        "-k" => meta().is_some_and(|m| m.permissions().mode() & 0o1000 != 0),
        "-p" => meta().is_some_and(|m| m.file_type().is_fifo()),
        "-r" => sys::access(arg, libc::R_OK),
        "-s" => meta().is_some_and(|m| m.len() > 0),
        "-S" => meta().is_some_and(|m| m.file_type().is_socket()),
        "-t" => arg.trim().parse::<i32>().is_ok_and(sys::isatty),
        "-u" => meta().is_some_and(|m| m.permissions().mode() & 0o4000 != 0),
        "-w" => sys::access(arg, libc::W_OK),
        "-x" => sys::access(arg, libc::X_OK),
        "-O" => meta().is_some_and(|m| m.uid() == sys::geteuid()),
        "-G" => meta().is_some_and(|m| m.gid() == sys::getgid()),
        "-N" => meta().is_some_and(|m| {
            m.mtime() > m.atime() || m.mtime() == m.atime() && m.mtime_nsec() > m.atime_nsec()
        }),
        "-z" => arg.is_empty(),
        "-n" => !arg.is_empty(),
        "-o" => shell.opts.get(arg).unwrap_or(false),
        "-v" => {
            // `-v name` or `-v name[index]`.
            match arg.find('[') {
                Some(open) if arg.ends_with(']') => {
                    let index = &arg[open + 1..arg.len() - 1];
                    if index == "@" || index == "*" {
                        shell.is_set(&arg[..open])
                    } else {
                        shell.get(arg).is_some()
                    }
                }
                _ => shell.get_value(arg).is_some(),
            }
        }
        "-R" => shell.vars.get(arg).is_some_and(|v| v.has(crate::vars::attr::NAMEREF)),
        _ => return None,
    })
}

/// Whether `op` is a binary test.
pub fn is_binary(op: &str) -> bool {
    matches!(
        op,
        "=" | "=="
            | "!="
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "-nt"
            | "-ot"
            | "-ef"
    )
}

/// A file comparison: `-nt`, `-ot`, `-ef`.
fn file_compare(op: &str, a: &str, b: &str) -> bool {
    let ma = std::fs::metadata(text::to_os(a)).ok();
    let mb = std::fs::metadata(text::to_os(b)).ok();
    let time = |m: &std::fs::Metadata| (m.mtime(), m.mtime_nsec());
    match op {
        "-nt" => match (ma, mb) {
            (Some(a), Some(b)) => time(&a) > time(&b),
            (Some(_), None) => true,
            _ => false,
        },
        "-ot" => match (ma, mb) {
            (Some(a), Some(b)) => time(&a) < time(&b),
            (None, Some(_)) => true,
            _ => false,
        },
        _ => match (ma, mb) {
            (Some(a), Some(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        },
    }
}

/// An integer of `test`: decimal, with blanks around it allowed.
fn test_integer(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix('-').or_else(|| trimmed.strip_prefix('+')).unwrap_or(trimmed);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok()
}

fn compare_integers(op: &str, a: i64, b: i64) -> bool {
    match op {
        "-eq" => a == b,
        "-ne" => a != b,
        "-lt" => a < b,
        "-le" => a <= b,
        "-gt" => a > b,
        _ => a >= b,
    }
}

// The test builtin.

/// Evaluating `test`'s arguments.
struct Test<'a> {
    shell: &'a Shell,
    args: &'a [String],
    pos: usize,
}

/// An error of `test`: its message (status 2).
type TestResult = Result<bool, String>;

impl Test<'_> {
    fn binary(&self, a: &str, op: &str, b: &str) -> TestResult {
        Ok(match op {
            "=" | "==" => a == b,
            "!=" => a != b,
            "<" => a.as_bytes() < b.as_bytes(),
            ">" => a.as_bytes() > b.as_bytes(),
            "-nt" | "-ot" | "-ef" => file_compare(op, a, b),
            _ => {
                let x =
                    test_integer(a).ok_or_else(|| format!("{a}: integer expression expected"))?;
                let y =
                    test_integer(b).ok_or_else(|| format!("{b}: integer expression expected"))?;
                compare_integers(op, x, y)
            }
        })
    }

    fn unary(&self, op: &str, arg: &str) -> TestResult {
        unary(self.shell, op, arg).ok_or_else(|| format!("{op}: unary operator expected"))
    }

    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    /// The general grammar, for more than four arguments.
    fn or(&mut self) -> TestResult {
        let mut value = self.and()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let right = self.and()?;
            value = value || right;
        }
        Ok(value)
    }

    fn and(&mut self) -> TestResult {
        let mut value = self.not()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let right = self.not()?;
            value = value && right;
        }
        Ok(value)
    }

    fn not(&mut self) -> TestResult {
        if self.peek() == Some("!") {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> TestResult {
        let Some(first) = self.peek().map(str::to_string) else {
            return Err("argument expected".into());
        };
        if first == "(" {
            self.pos += 1;
            let value = self.or()?;
            if self.peek() != Some(")") {
                return Err("`)' expected".into());
            }
            self.pos += 1;
            return Ok(value);
        }
        // A binary operator after this argument.
        if let Some(op) = self.args.get(self.pos + 1)
            && is_binary(op)
            && self.pos + 2 < self.args.len()
        {
            let b = self.args[self.pos + 2].clone();
            self.pos += 3;
            return self.binary(&first, op, &b);
        }
        if first.starts_with('-')
            && first.len() == 2
            && unary(self.shell, &first, "").is_some()
            && self.pos + 1 < self.args.len()
        {
            let arg = self.args[self.pos + 1].clone();
            self.pos += 2;
            return self.unary(&first, &arg);
        }
        self.pos += 1;
        Ok(!first.is_empty())
    }

    /// POSIX's rules by the number of arguments.
    fn by_count(&mut self, args: &[String]) -> TestResult {
        let a = |i: usize| args[i].as_str();
        match args.len() {
            0 => Ok(false),
            1 => Ok(!args[0].is_empty()),
            2 => {
                if a(0) == "!" {
                    return Ok(args[1].is_empty());
                }
                if a(0).starts_with('-') && a(0).len() == 2 {
                    return self.unary(a(0), a(1));
                }
                Err(format!("{}: unary operator expected", a(0)))
            }
            3 => {
                if is_binary(a(1)) {
                    return self.binary(a(0), a(1), a(2));
                }
                if a(1) == "-a" || a(1) == "-o" {
                    let (x, y) = (!args[0].is_empty(), !args[2].is_empty());
                    return Ok(if a(1) == "-a" { x && y } else { x || y });
                }
                if a(0) == "!" {
                    return self.by_count(&args[1..]).map(|v| !v);
                }
                if a(0) == "(" && a(2) == ")" {
                    return Ok(!args[1].is_empty());
                }
                Err(format!("{}: binary operator expected", a(1)))
            }
            4 => {
                if a(0) == "!" {
                    return self.by_count(&args[1..]).map(|v| !v);
                }
                if a(0) == "(" && a(3) == ")" {
                    return self.by_count(&args[1..3]);
                }
                self.general()
            }
            _ => self.general(),
        }
    }

    fn general(&mut self) -> TestResult {
        let value = self.or()?;
        if self.pos < self.args.len() {
            return Err("too many arguments".into());
        }
        Ok(value)
    }
}

/// `test expression` and `[ expression ]`.
pub fn test_builtin(shell: &mut Shell, args: &[String]) -> Res {
    let name = args[0].as_str();
    let mut operands = &args[1..];
    if name == "[" {
        match operands.last() {
            Some(last) if last == "]" => operands = &operands[..operands.len() - 1],
            _ => {
                shell.error("[: missing `]'");
                return Ok(2);
            }
        }
    }
    // `-v 'a[$i]'`: the subscript is expanded, as in bash.
    let mut expanded = None;
    for (i, pair) in operands.windows(2).enumerate() {
        if pair[0] == "-v" && pair[1].contains('[') {
            let element = shell.expand_element(&pair[1])?;
            if element != pair[1] {
                expanded.get_or_insert_with(|| operands.to_vec())[i + 1] = element;
            }
        }
    }
    let operands = expanded.as_deref().unwrap_or(operands);
    let mut test = Test { shell, args: operands, pos: 0 };
    match test.by_count(operands) {
        Ok(value) => Ok(i32::from(!value)),
        Err(message) => {
            shell.error(&format!("{name}: {message}"));
            Ok(2)
        }
    }
}

// [[ ... ]].

impl Shell {
    /// Runs `[[ ... ]]`: 0 if true, 1 if false, 2 for an error.
    pub fn eval_cond(&mut self, expr: &CondExpr) -> Res {
        match self.cond(expr) {
            Ok(true) => Ok(0),
            Ok(false) => Ok(1),
            Err(CondError::Message(message)) => {
                self.error(&message);
                Ok(2)
            }
            Err(CondError::Flow(flow)) => Err(flow),
        }
    }

    fn cond(&mut self, expr: &CondExpr) -> Result<bool, CondError> {
        Ok(match expr {
            CondExpr::And(a, b) => self.cond(a)? && self.cond(b)?,
            CondExpr::Or(a, b) => self.cond(a)? || self.cond(b)?,
            CondExpr::Not(a) => !self.cond(a)?,
            CondExpr::Group(a) => self.cond(a)?,
            CondExpr::Word(word) => !self.expand_string(word)?.is_empty(),
            CondExpr::Unary(op, word) => {
                let mut arg = self.expand_string(word)?;
                if op == "-v" {
                    arg = self.expand_element(&arg)?;
                }
                unary(self, op, &arg)
                    .ok_or_else(|| CondError::Message(format!("{op}: unary operator expected")))?
            }
            CondExpr::Binary(left, op, right) => {
                let a = self.expand_string(left)?;
                match op.as_str() {
                    "==" | "=" | "!=" => {
                        let pattern = self.expand_pattern(right)?;
                        let mut options = self.pattern_options();
                        options.extglob = true;
                        options.nocase = self.shopt("nocasematch");
                        let matched = Pattern::new(&pattern, options).matches(&a);
                        if op == "!=" { !matched } else { matched }
                    }
                    "=~" => {
                        let regex = self.expand_regex(right)?;
                        match regex_match(&regex, &a, self.shopt("nocasematch")) {
                            Ok(Some(groups)) => {
                                self.rematch = groups;
                                true
                            }
                            Ok(None) => {
                                self.rematch.clear();
                                false
                            }
                            Err(message) => {
                                self.rematch.clear();
                                return Err(CondError::Message(message));
                            }
                        }
                    }
                    "<" => a.as_bytes() < self.expand_string(right)?.as_bytes(),
                    ">" => a.as_bytes() > self.expand_string(right)?.as_bytes(),
                    "-nt" | "-ot" | "-ef" => {
                        let b = self.expand_string(right)?;
                        file_compare(op, &a, &b)
                    }
                    _ => {
                        // Inside [[ ]], the integers are arithmetic; an
                        // error makes the comparison false, as in bash.
                        let b = self.expand_string(right)?;
                        match (self.arith_command("[[", &a), self.arith_command("[[", &b)) {
                            (Some(x), Some(y)) => compare_integers(op, x, y),
                            _ => false,
                        }
                    }
                }
            }
        })
    }
}

enum CondError {
    Message(String),
    Flow(Flow),
}

impl From<Flow> for CondError {
    fn from(flow: Flow) -> CondError {
        CondError::Flow(flow)
    }
}

// Regular expressions, through the C library.

/// `regex_t`, which the C libraries lay out differently: room for any.
#[repr(C, align(16))]
struct RegexBuf([u8; 256]);

#[cfg(target_env = "gnu")]
type RegOff = i32;
#[cfg(not(target_env = "gnu"))]
type RegOff = isize;

#[repr(C)]
#[derive(Clone, Copy)]
struct RegMatch {
    rm_so: RegOff,
    rm_eo: RegOff,
}

const REG_EXTENDED: c_int = 1;
const REG_ICASE: c_int = 2;

unsafe extern "C" {
    fn regcomp(preg: *mut c_void, pattern: *const c_char, cflags: c_int) -> c_int;
    fn regexec(
        preg: *const c_void,
        string: *const c_char,
        nmatch: usize,
        pmatch: *mut RegMatch,
        eflags: c_int,
    ) -> c_int;
    fn regerror(code: c_int, preg: *const c_void, buf: *mut c_char, size: usize) -> usize;
    fn regfree(preg: *mut c_void);
}

/// Matches `text` against the extended regular expression `pattern`: the
/// match and its groups (`BASH_REMATCH`), or `None`.
pub fn regex_match(pattern: &str, text: &str, nocase: bool) -> Result<Option<Vec<String>>, String> {
    let c_pattern = CString::new(text::encode(pattern))
        .map_err(|_| "invalid regular expression".to_string())?;
    let bytes = text::encode(text);
    let c_text = CString::new(bytes.clone()).unwrap_or_default();
    let mut buf = RegexBuf([0; 256]);
    let flags = REG_EXTENDED | if nocase { REG_ICASE } else { 0 };
    // SAFETY: buf has room for any C library's regex_t; the pattern is a
    // C string.
    let code = unsafe { regcomp(buf.0.as_mut_ptr().cast(), c_pattern.as_ptr(), flags) };
    if code != 0 {
        let mut message = [0 as c_char; 256];
        // SAFETY: regerror writes at most message.len() bytes.
        unsafe { regerror(code, buf.0.as_ptr().cast(), message.as_mut_ptr(), message.len()) };
        // SAFETY: regerror wrote a terminated string.
        let text =
            unsafe { std::ffi::CStr::from_ptr(message.as_ptr()) }.to_string_lossy().into_owned();
        return Err(format!("{pattern}: {text}"));
    }
    // Room for every group the pattern could have.
    let groups = pattern.matches('(').count() + 1;
    let mut matches = vec![RegMatch { rm_so: -1, rm_eo: -1 }; groups];
    // SAFETY: buf holds a compiled regex; matches has `groups` entries.
    let found =
        unsafe { regexec(buf.0.as_ptr().cast(), c_text.as_ptr(), groups, matches.as_mut_ptr(), 0) };
    // SAFETY: buf holds a compiled regex, freed once.
    unsafe { regfree(buf.0.as_mut_ptr().cast()) };
    if found != 0 {
        return Ok(None);
    }
    // The groups the pattern really has: the C libraries mark the rest -1.
    let mut out = Vec::new();
    let mut last = 0;
    for (i, m) in matches.iter().enumerate() {
        if m.rm_so >= 0 {
            last = i;
        }
    }
    for m in &matches[..=last] {
        if m.rm_so < 0 {
            out.push(String::new());
        } else {
            out.push(text::decode(&bytes[m.rm_so as usize..m.rm_eo as usize]));
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test(args: &[&str]) -> i32 {
        let mut shell = Shell::new("test", false);
        let args: Vec<String> =
            std::iter::once("test").chain(args.iter().copied()).map(String::from).collect();
        test_builtin(&mut shell, &args).unwrap()
    }

    #[test]
    fn test_follows_posix() {
        assert_eq!(test(&[]), 1);
        assert_eq!(test(&["x"]), 0);
        assert_eq!(test(&[""]), 1);
        assert_eq!(test(&["!", ""]), 0);
        assert_eq!(test(&["-n", "a"]), 0);
        assert_eq!(test(&["a", "=", "a"]), 0);
        assert_eq!(test(&["2", "-lt", "10"]), 0);
        assert_eq!(test(&["abc", "-lt", "1"]), 2);
        assert_eq!(test(&["(", "a", ")"]), 0);
        assert_eq!(test(&["a", "=", "b", "-o", "c", "=", "c"]), 0);
        assert_eq!(test(&["!", "a", "=", "a"]), 1);
        assert_eq!(test(&["-d", "/"]), 0);
    }

    #[test]
    fn regular_expressions_capture() {
        let groups = regex_match("^([a-z]+)-([0-9]+)$", "abc-123", false).unwrap().unwrap();
        assert_eq!(groups, ["abc-123", "abc", "123"]);
        assert_eq!(regex_match("^x", "abc", false).unwrap(), None);
        assert!(regex_match("(", "abc", false).is_err());
    }
}
