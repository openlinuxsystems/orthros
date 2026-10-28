// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The kernel command line.
//!
//! GRUB passes everything after the kernel path on its `multiboot2` line.
//! Flags are separated by spaces and are either a bare name (`kdb`) or
//! `name=value` (`loglevel=debug`). Unknown flags are reported and
//! otherwise ignored. A lone `--` ends the flags: the words after it are
//! the first program's arguments ([`CommandLine::init_args`]), as on
//! Linux. Every flag in [`FLAGS`] is documented in dukovo-boot(7); `ob
//! docs --check` enforces that.

/// One flag the kernel understands.
#[derive(Clone, Copy, Debug)]
pub struct Flag {
    /// Name before any `=`.
    pub name: &'static str,
    /// Placeholder for the value, or `None` for a bare flag.
    pub value: Option<&'static str>,
    /// One-line description.
    pub summary: &'static str,
}

/// Every flag the kernel understands.
pub const FLAGS: &[Flag] = &[
    Flag {
        name: "loglevel",
        value: Some("level"),
        summary: "lowest message level shown on the consoles: error, warn, info or debug",
    },
    Flag { name: "kdb", value: None, summary: "start the kernel debugger on the serial port" },
    Flag { name: "trace", value: None, summary: "record trace points from the start of boot" },
    Flag {
        name: "panic",
        value: Some("seconds"),
        summary: "restart this many seconds after a panic instead of waiting for a key",
    },
    Flag { name: "nosmp", value: None, summary: "start only the boot CPU" },
    Flag {
        name: "nokaslr",
        value: None,
        summary: "keep the kernel, the direct map and the mapping window at fixed addresses",
    },
    Flag {
        name: "nopcid",
        value: None,
        summary: "do not tag TLB entries with process-context identifiers (PCIDs)",
    },
    Flag {
        name: "nox2apic",
        value: None,
        summary: "drive the local APIC in xAPIC mode even if x2APIC is available",
    },
    Flag { name: "mce", value: Some("off"), summary: "do not turn machine-check reporting on" },
    Flag {
        name: "usb",
        value: Some("off"),
        summary: "leave the USB controllers as the firmware left them",
    },
    Flag {
        name: "mce-poll",
        value: Some("seconds"),
        summary: "read the CPUs' machine-check banks this often (300 by default)",
    },
    Flag {
        name: "font",
        value: Some("size"),
        summary: "framebuffer console font: 8x16, 12x24 or 16x32",
    },
    Flag {
        name: "crashlog",
        value: Some("off"),
        summary: "do not keep the log in memory for the next boot",
    },
    Flag {
        name: "selftest",
        value: Some("name"),
        summary: "run a self-test at the end of boot: page-fault, crash-reboot, breakpoint, \
                  mm-stress, stack-overflow, smep, smap, tlb, caps, fpu, sched-fair, sched-rt, \
                  sched-pi, sched-balance, lockdep, rcu, oom, user, ipc, fuzz or bench",
    },
    Flag {
        name: "fuzz-seed",
        value: Some("number"),
        summary: "the first seed selftest=fuzz gives its programs, to repeat a run; random by \
                  default",
    },
    Flag {
        name: "lockdep",
        value: Some("on|off"),
        summary: "turn the lock-order checker on or off; it is on by default in debug builds only",
    },
    Flag {
        name: "init",
        value: Some("path"),
        summary: "the first program, from the boot archive (/system/commands/spark if not given); \
                  the words after a lone -- are its arguments",
    },
    Flag {
        name: "qemu-exit",
        value: None,
        summary: "leave QEMU through isa-debug-exit when boot finishes or the kernel panics",
    },
];

/// The self-tests `selftest=name` runs; radix-debug(7) describes them.
pub const SELFTESTS: &[&str] = &[
    "page-fault",
    "crash-reboot",
    "breakpoint",
    "mm-stress",
    "stack-overflow",
    "smep",
    "smap",
    "tlb",
    "caps",
    "fpu",
    "sched-fair",
    "sched-rt",
    "sched-pi",
    "sched-balance",
    "lockdep",
    "rcu",
    "oom",
    "user",
    "ipc",
    "fuzz",
    "bench",
];

/// A parsed view of the command line.
#[derive(Clone, Copy, Debug)]
pub struct CommandLine<'a> {
    text: &'a str,
}

impl<'a> CommandLine<'a> {
    /// Wraps the raw command line.
    pub fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// The raw text.
    pub fn as_str(&self) -> &'a str {
        self.text
    }

    /// Every flag as (name, value): the words up to a lone `--`.
    pub fn flags(&self) -> impl Iterator<Item = (&'a str, Option<&'a str>)> + 'a {
        self.text.split_ascii_whitespace().take_while(|&word| word != "--").map(|flag| {
            match flag.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (flag, None),
            }
        })
    }

    /// The words after a lone `--`: the first program's arguments.
    pub fn init_args(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.text.split_ascii_whitespace().skip_while(|&word| word != "--").skip(1)
    }

    /// Whether the bare flag `name` is present.
    pub fn has(&self, name: &str) -> bool {
        self.flags().any(|(flag, value)| flag == name && value.is_none())
    }

    /// The value of the last `name=value` flag.
    pub fn value(&self, name: &str) -> Option<&'a str> {
        self.flags().filter(|&(flag, _)| flag == name).filter_map(|(_, value)| value).last()
    }

    /// Flags the kernel does not know, for a warning at boot.
    pub fn unknown(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.flags()
            .map(|(name, _)| name)
            .filter(|name| !FLAGS.iter().any(|flag| flag.name == *name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_values() {
        let line = CommandLine::new("  qemu-exit loglevel=debug kdb  loglevel=warn x=1 ");
        assert!(line.has("qemu-exit"));
        assert!(line.has("kdb"));
        assert!(!line.has("loglevel"));
        assert_eq!(line.value("loglevel"), Some("warn"));
        assert_eq!(line.value("kdb"), None);
        assert_eq!(line.unknown().collect::<Vec<_>>(), ["x"]);
        assert_eq!(line.init_args().count(), 0);
    }

    #[test]
    fn words_after_a_lone_dash_dash_are_arguments() {
        let line = CommandLine::new("kdb init=/a -- /system/tests/hello -- kdb x=1");
        assert!(line.has("kdb"));
        assert_eq!(line.value("init"), Some("/a"));
        assert_eq!(line.unknown().count(), 0);
        let args: Vec<_> = line.init_args().collect();
        assert_eq!(args, ["/system/tests/hello", "--", "kdb", "x=1"]);
    }

    #[test]
    fn flag_names_are_unique_and_simple() {
        for (i, flag) in FLAGS.iter().enumerate() {
            assert!(
                flag.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            );
            assert!(FLAGS[i + 1..].iter().all(|other| other.name != flag.name));
        }
    }

    #[test]
    fn selftest_summary_names_every_test() {
        let flag = FLAGS.iter().find(|flag| flag.name == "selftest").unwrap();
        for name in SELFTESTS {
            assert!(flag.summary.contains(name), "the selftest flag does not mention {name}");
        }
    }
}
