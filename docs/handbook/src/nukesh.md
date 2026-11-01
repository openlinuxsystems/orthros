<!--
SPDX-FileCopyrightText: 2026 The Dukovo contributors
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# Nukesh guide and scripting

Nukesh is Dukovo's shell: the one you type at, and `/bin/sh`, which runs
the system's scripts. It is the POSIX shell with the parts of bash that
scripts use, so a script written for bash runs unchanged and prints what
bash prints. This chapter shows how to use it; `nukesh(1)` is the full
reference, and `help` describes each builtin.

## Getting a shell

Dukovo boots to a shell on the screen. In QEMU, start it with:

```sh
./ob run
```

QEMU's window shows the first virtual console, and the terminal you
started `ob` from shows the kernel's log on the serial port. The console
shows `/etc/issue` and a prompt:

```text
Dukovo Server 0.11.0 (tty1)

#
```

There are six consoles, `tty1` to `tty6`: Alt+F1 to Alt+F6 show each,
Shift+Page Up looks back through the last 1000 lines, and
Ctrl+Alt+Delete shows a menu to sign in, restart or power off. Each
console asks for a name and a password and runs that person's login
shell (the handbook's users chapter); `exit` signs out. `consoled(8)`
describes the consoles and their keys.

## Typing commands

Lines are typed in a line editor with Emacs keys (`set -o vi` gives vi
keys). The up and down arrows go through the history, which is kept in
`~/.nukesh_history`, and Ctrl+R searches it. A command that is not
finished, such as an open quote or an `if` without its `fi`, goes on to
the next line.

Tab completes command names, file names and variables. Every program
in `/system/commands` has a completion file, made from its manual page, so
`grep --` and Tab shows a menu of `grep`'s options with what each does,
and the word after an option that takes a file, a directory or a user
completes as one.

Jobs work as in bash: `command &` runs one in the background, Ctrl+Z
stops the one in the foreground, and `jobs`, `fg`, `bg` and `kill %1`
manage them. Ctrl+C drops the line being typed.

## Colours

The line is coloured as you type it: commands green, options cyan,
quoted text yellow, variables magenta, pipes and `&&` yellow. A command
that does not exist is red and underlined before you press Enter, and
if you run it anyway, Nukesh names the closest command it knows: `sevice:
command not found. Did you mean service?`. A command from your history
that starts with what you typed shows faded after the cursor; → or End
takes it.

The prompt is `name@host folder $`. After a command fails it shows a
`✗ 127` badge with the exit status, and while you are root it shows an
`admin` badge and `#`. `ls`, `grep`, `diff` and `ip` are in colour too,
but only on the terminal: piped into another command or a file, their
output is plain.

Every colour can be changed in `/system/settings/nukesh/colors.toml`, or
for yourself in `~/.config/nukesh/colors.toml` (colors.toml(5)), and
`nukesh --colors` shows them all. Only the terminal's 16 standard colours
are used, so they follow its theme. `NO_COLOR=1`, or `enabled = false`
in the file, turns colour off.

## Finding your way

Programs are in `/system/commands`, which `/bin` and `/sbin` lead to; `ls
/bin` lists them. Each has a manual page:

```sh
man ls          # one page
man -k process  # pages whose names or descriptions mention a word
help read       # a builtin of the shell
```

The commands are the ones a Linux server has:

- files and text: uutils coreutils (`ls`, `cp`, `sort` and the rest),
  `find` and `xargs`, `diff`, `grep`, `sed`, `awk`, `less`, `file`,
  `tree`, `hexdump` and `xxd`;
- editors and sessions: `nano`, `vi`, `tmux`, `script`;
- archives: `tar`, `cpio` and `unzip` from libarchive, `gzip`, `bzip2`,
  `xz`, `zstd`, `lz4`, `7zz` and `zip`;
- data: `jq`, `bc`, `patch`, `column`;
- the system: `ps`, `top`, `free`, `df`, `uptime`, `w`, `vmstat`,
  `pidstat`, `dmesg`, `lsof`, `kill` and `pgrep`, `renice`, `ionice`,
  `taskset` and `watch`, written for Dukovo with the Linux tools' names,
  options and output (`systools(1)`);
- services and power: `service`, `shutdown`, `reboot` and `poweroff`
  (`spark(8)`);
- tracing: `rtrace`, which shows a program's system calls and messages
  as `strace` shows Linux's.

Files are described in the next chapter, Files, the directory hierarchy
and /volumes.

## Scripts

A script whose first line is `#!/bin/sh` runs in POSIX mode, as dash
would run it; one that starts with `#!/bin/bash` or `#!/usr/bin/env
bash` gets the bash features: arrays and associative arrays, `[[ ]]`,
`(( ))`, `local`, `mapfile`, `printf -v`, here-strings, process
substitution, `set -euo pipefail` and traps on `EXIT`, `ERR` and
signals.

```bash
#!/bin/bash
set -euo pipefail
declare -A size
for f in /system/settings/*; do
    [[ -f $f ]] && size[$f]=$(wc -c < "$f")
done
for f in "${!size[@]}"; do
    printf '%8d %s\n' "${size[$f]}" "$f"
done
```

Two options help while writing one:

```sh
nukesh -n script.sh   # check the syntax, run nothing
nukesh -x script.sh   # show each command, with its file and line
```

## Where Nukesh differs from bash

Nukesh's test suite checks each of its cases against the output of
bash 5.2, so differences are bugs, with a few deliberate ones that
`nukesh(1)` lists under LIMITS. Text is UTF-8 in every locale.
Patterns are matched in time that grows with the text and the pattern,
never exponentially. Commands nest at most 1,000 deep and functions call
each other at most 2,000 deep, where bash would run out of stack.
`$NUKESH_VERSION` tells the two shells apart; `$BASH_VERSION` is bash
5.2's, so that scripts that check it run.
