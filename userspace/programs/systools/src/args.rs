// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Command-line arguments, read as getopt_long(3) reads them: `-abc` is
//! `-a -b -c`, an option's value follows it (`-n5`, `-n 5`, `--lines=5`,
//! `--lines 5`), `--` ends the options, and `-` alone is an operand.
//! Options and operands may mix, unless a tool asks for the options to
//! end at the first operand ([`Args::stop_at_operand`]), as tools that run
//! a command do.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::util::{Fail, usage};

/// One option or operand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arg {
    /// `-x`.
    Short(char),
    /// `--name`, without its `=value`.
    Long(String),
    /// An operand.
    Operand(OsString),
}

/// A tool's arguments, as it reads them.
pub struct Args {
    /// The tool's name, for messages.
    pub tool: &'static str,
    items: Vec<OsString>,
    /// The next item to read.
    next: usize,
    /// The rest of a cluster of short options being read, and where.
    cluster: Option<(Vec<u8>, usize)>,
    /// The value after a long option's `=`.
    long_value: Option<OsString>,
    /// Operands put aside while options were read after them.
    operands: Vec<OsString>,
    /// No more options: everything left is an operand.
    ended: bool,
    stop_at_operand: bool,
}

impl Args {
    /// The arguments after the tool's name.
    pub fn new(tool: &'static str, items: Vec<OsString>) -> Args {
        Args {
            tool,
            items,
            next: 0,
            cluster: None,
            long_value: None,
            operands: Vec::new(),
            ended: false,
            stop_at_operand: false,
        }
    }

    /// The subcommand of a tool that has them, `start` of `scrub start
    /// -B /`: the first argument, unless it is an option. It is read
    /// before any other, since [`Args::next`] gives the options first.
    pub fn subcommand(&mut self) -> Option<OsString> {
        let item =
            self.items.get(self.next).filter(|item| item.as_bytes().first() != Some(&b'-'))?;
        let item = item.clone();
        self.next += 1;
        Some(item)
    }

    /// Ends the options at the first operand, as POSIX getopt does: what
    /// follows a command's name is the command's.
    pub fn stop_at_operand(&mut self) {
        self.stop_at_operand = true;
    }

    /// The next option or operand.
    pub fn next(&mut self) -> Result<Option<Arg>, Fail> {
        if let Some(value) = self.long_value.take() {
            let name = self.last_long();
            return Err(usage(format!(
                "option '--{name}' takes no value (got '{}')",
                value.to_string_lossy()
            )));
        }
        if let Some((bytes, at)) = self.cluster.take()
            && at < bytes.len()
        {
            return Ok(Some(self.short_at(bytes, at)));
        }
        if !self.operands.is_empty() && (self.ended || self.next >= self.items.len()) {
            return Ok(Some(Arg::Operand(self.operands.remove(0))));
        }
        let Some(item) = self.items.get(self.next).cloned() else { return Ok(None) };
        self.next += 1;
        let bytes = item.as_bytes();
        if self.ended || bytes.len() < 2 || bytes[0] != b'-' {
            if self.stop_at_operand && !self.ended {
                self.ended = true;
            }
            if !self.ended && !self.stop_at_operand {
                // Read on for options after it; it comes back after them.
                self.operands.push(item);
                return self.next();
            }
            return Ok(Some(Arg::Operand(item)));
        }
        if bytes == b"--" {
            self.ended = true;
            return self.next();
        }
        if let Some(long) = bytes.strip_prefix(b"--") {
            let (name, value) = match long.iter().position(|&b| b == b'=') {
                Some(eq) => (&long[..eq], Some(OsStr::from_bytes(&long[eq + 1..]).to_owned())),
                None => (long, None),
            };
            self.long_value = value;
            return Ok(Some(Arg::Long(String::from_utf8_lossy(name).into_owned())));
        }
        Ok(Some(self.short_at(bytes.to_vec(), 1)))
    }

    /// The short option at `at` in a cluster.
    fn short_at(&mut self, bytes: Vec<u8>, at: usize) -> Arg {
        let rest = String::from_utf8_lossy(&bytes[at..]).into_owned();
        let c = rest.chars().next().unwrap_or('?');
        let next = at + c.len_utf8();
        self.cluster = Some((bytes, next));
        Arg::Short(c)
    }

    /// The name of the long option just read, for messages.
    fn last_long(&self) -> String {
        let item = self.items.get(self.next.wrapping_sub(1)).map(|i| i.to_string_lossy());
        let item = item.unwrap_or_default();
        let name = item.trim_start_matches('-');
        name.split('=').next().unwrap_or("").to_string()
    }

    /// The value of the option just read: the rest of its cluster, what
    /// follows its `=`, or the next argument.
    pub fn value(&mut self) -> Result<OsString, Fail> {
        if let Some(value) = self.long_value.take() {
            return Ok(value);
        }
        if let Some((bytes, at)) = self.cluster.take()
            && at < bytes.len()
        {
            return Ok(OsString::from_vec(bytes[at..].to_vec()));
        }
        match self.items.get(self.next) {
            Some(item) => {
                self.next += 1;
                Ok(item.clone())
            }
            None => Err(usage("an option needs a value".into())),
        }
    }

    /// The value of the option just read, as text.
    pub fn text(&mut self) -> Result<String, Fail> {
        let value = self.value()?;
        value.into_string().map_err(|v| usage(format!("'{}' is not UTF-8", v.to_string_lossy())))
    }

    /// The value of the option just read, as a number of type `T`.
    pub fn number<T: std::str::FromStr>(&mut self) -> Result<T, Fail> {
        let text = self.text()?;
        text.trim().parse().map_err(|_| usage(format!("'{text}' is not a valid number")))
    }

    /// The value of a short option that takes one only when it is joined
    /// to it (`-ofile`, as for strace's `-tt`): none if the cluster ends.
    pub fn joined_value(&mut self) -> Option<OsString> {
        let (bytes, at) = self.cluster.take()?;
        if at < bytes.len() { Some(OsString::from_vec(bytes[at..].to_vec())) } else { None }
    }

    /// The value of a short option that may have one (`-U [user]`): the
    /// rest of its cluster, or the next argument if it is no option and
    /// `wanted` takes it; else none.
    pub fn optional_operand(&mut self, wanted: impl Fn(&OsStr) -> bool) -> Option<OsString> {
        if let Some(value) = self.joined_value() {
            return Some(value);
        }
        let item = self.items.get(self.next)?;
        if self.ended || item.as_bytes().first() == Some(&b'-') || !wanted(item) {
            return None;
        }
        self.next += 1;
        Some(item.clone())
    }

    /// The value of an option that may have one, as journalctl's `-b`
    /// (`-b`, `-b -1`, `--boot=-1`): what follows its `=` or the rest of
    /// its cluster, or the next argument if `wanted` takes it, even one
    /// that starts with `-`; else none.
    pub fn optional_value_where(&mut self, wanted: impl Fn(&OsStr) -> bool) -> Option<OsString> {
        if let Some(value) = self.long_value.take() {
            return Some(value);
        }
        if let Some(value) = self.joined_value() {
            return Some(value);
        }
        let item = self.items.get(self.next)?;
        if self.ended || !wanted(item) {
            return None;
        }
        self.next += 1;
        Some(item.clone())
    }

    /// Takes a long option's value if it has one (`--color=never`), or none
    /// (`--color`).
    pub fn optional_value(&mut self) -> Option<OsString> {
        self.long_value.take()
    }

    /// Every argument not read yet, as operands, for a command to run.
    pub fn rest(&mut self) -> Vec<OsString> {
        let mut rest = std::mem::take(&mut self.operands);
        rest.extend(self.items.drain(self.next.min(self.items.len())..));
        self.next = 0;
        self.items.clear();
        rest
    }

    /// Fails for an option the tool does not know.
    // A method, so that a tool's option loop reads `args.unknown(&other)`.
    #[allow(clippy::unused_self)]
    pub fn unknown(&self, arg: &Arg) -> Fail {
        match arg {
            Arg::Short(c) => usage(format!("invalid option -- '{c}'")),
            Arg::Long(name) => usage(format!("unrecognized option '--{name}'")),
            Arg::Operand(operand) => {
                usage(format!("unexpected argument '{}'", operand.to_string_lossy()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Args {
        Args::new("test", items.iter().map(OsString::from).collect())
    }

    #[test]
    fn a_subcommand_comes_before_its_options() {
        let mut a = args(&["start", "-B", "/"]);
        assert_eq!(a.subcommand(), Some(OsString::from("start")));
        assert_eq!(a.next().unwrap(), Some(Arg::Short('B')));
        assert_eq!(a.next().unwrap(), Some(Arg::Operand("/".into())));
        assert_eq!(a.next().unwrap(), None);
        assert_eq!(args(&["-B", "start"]).subcommand(), None);
        assert_eq!(args(&[]).subcommand(), None);
    }

    #[test]
    fn clusters_values_and_operands() {
        let mut a = args(&["-ab", "file", "-n5", "--lines=7", "--", "-x"]);
        assert_eq!(a.next().unwrap(), Some(Arg::Short('a')));
        assert_eq!(a.next().unwrap(), Some(Arg::Short('b')));
        assert_eq!(a.next().unwrap(), Some(Arg::Short('n')));
        assert_eq!(a.text().unwrap(), "5");
        assert_eq!(a.next().unwrap(), Some(Arg::Long("lines".into())));
        assert_eq!(a.number::<u32>().unwrap(), 7);
        assert_eq!(a.next().unwrap(), Some(Arg::Operand("file".into())));
        assert_eq!(a.next().unwrap(), Some(Arg::Operand("-x".into())));
        assert_eq!(a.next().unwrap(), None);
    }

    #[test]
    fn separate_values_and_commands() {
        let mut a = args(&["-n", "2", "ls", "-l"]);
        a.stop_at_operand();
        assert_eq!(a.next().unwrap(), Some(Arg::Short('n')));
        assert_eq!(a.text().unwrap(), "2");
        assert_eq!(a.next().unwrap(), Some(Arg::Operand("ls".into())));
        assert_eq!(a.rest(), vec![OsString::from("-l")]);
    }

    #[test]
    fn long_options_without_values_refuse_them() {
        let mut a = args(&["--all=yes"]);
        assert_eq!(a.next().unwrap(), Some(Arg::Long("all".into())));
        assert!(a.next().is_err());
    }
}
