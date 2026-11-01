authd(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

authd - keep the accounts and check passwords

# SYNOPSIS

*authd*

# DESCRIPTION

authd keeps Dukovo's accounts and their passwords (ADR 0218). Spark runs
it as the *authd* service. It alone changes the account files, and it
alone reads the passwords' hashes, it alone starts the sessions of
people who sign in, and it alone runs commands as an administrator:
other programs ask it over a socket, as *login*(1), *user*(8),
*passwd*(1) and *elevate*(8) do.

An account is a name its owner chooses and a password, which may be
empty: nothing else, and never an age, a birth date or an identity
(ADR 0126). Each lives in three files:

- _/system/settings/passwd_ and _/system/settings/group_, in Linux's
  formats, which every program reads (*getpwnam*(3), *ls -l*, *id*(1))
  and which hold no password;
- _/system/state/authd/passwords_, a line _name_*:*_hash_*:* for each
  account, as Linux's _/etc/shadow_ begins, root's and of mode 0600 in a
  folder of mode 0700, which no other service's view holds
  (*service.toml*(5)).

A hash is Argon2id (RFC 9106), at the RFC's second recommendation: 64
MiB of memory, 3 passes and 4 lanes, with a salt of 16 random bytes,
written as a PHC string, as libxcrypt and Python's argon2-cffi write it:

```
alice:$argon2id$v=19$m=65536,t=3,p=4$c29tZXNhbHQ...$GpZ3sK/oH9p7...:
```

A hash says its cost, so one moved from Linux, or made at another cost,
is checked all the same, up to 1 GiB, 16 passes and 16 lanes. An empty
hash is an empty password; an account with no line, or a line authd
cannot read, is locked: no password opens it.
authd writes each file anew beside the old one, syncs it and renames it
over the old one, the hashes first and the passwd file last, so that an
account is there once its user is. Lines it does not read, an
administrator's comments among them, are kept as they are.

No rules are forced on a password (ADR 0218): *user* and *passwd* warn
about an empty one, or one shorter than 8 characters, and ask whether to
keep it; the person who answers decides.

# THE SOCKET

authd answers at _/system/live/authd/socket_, which anyone may connect
to. A request is its fields, each ending in a NUL byte, after which the
client shuts down its end for writing; authd answers with one line,
*ok* or *ok* _text_ when it did what was asked, or *no* _why_, then
closes the connection, but for a session (below):

*check* _name_ _password_
	Whether _password_ is _name_'s. Root asks this of any account;
	another program only of the account it runs for, as a screen lock
	does, and gets *no* for any other name, there or not.

*passwd* _name_ _current_ _new_
	Sets _name_'s password to _new_. Root changes anyone's, with an
	empty _current_; a user changes only their own, given the one it
	has.

*add* _name_ _full-name_ *admin*|*user* _password_
	Makes an account (below). Only root may.

*needs-owner*
	*ok yes* if no account can be signed in to yet, which is the case
	while there is no person's account (a number from 1000) and root's
	is locked, with no password line or one authd cannot read; *ok no*
	if one can.

*owner* _name_ _full-name_ _password_
	Makes the owner's account, an administrator's, at a machine's first
	sign-in. Only root may, and only while *needs-owner* says *yes*.

*reset* _name_ _password_
	Sets administrator _name_'s password to _password_, which may be
	empty, from the startup-trouble menu of *login*(1) (ADR 0253). Only
	root may, and only while the kernel was started with
	*startup-trouble* (*dukovo-boot*(7)); an account that is not an
	administrator's is refused, as an administrator changes its
	password with *elevate passwd*. The wrong passwords given for
	_name_ are forgotten, so the new one opens the account at once, and
	a disabled account is enabled. It is a security event, and each
	administrator is told at their next sign-in (see SESSIONS).

*disable* _name_
	Disables the person's account _name_ (ADR 0220): marks it with *!*
	before its hash, as Linux's *usermod -L*, keeping the password;
	ends the commands authd started for it through *elevate*(8) and
	forgets the panes unlocked for it; and has the kernel end the user
	(*radix-names*(2)'s *end*), again until no process of it is left.
	Its sessions and everything it runs end, and a directory it handed
	to another program opens nothing more. Answers *ok* and how many of
	its processes ended. Only root may. A disabled account's right
	password is answered *no the account* _name_ *is disabled*, which
	only someone who knows it learns; a wrong one as any wrong one.

*enable* _name_
	Takes the mark away: the account signs in again with the password
	it had. Only root may.

*remove* _name_
	Disables and ends the account as *disable* does, adds its number to
	the retired ones, which no account gets again, and takes its user,
	its own group and its name in other groups out of the account files.
	Answers *ok* and its home, which authd leaves as it is: *user*(8)
	keeps, archives or deletes it. Only root may.

*rename* _name_ _new_
	Renames the account, its own group, its name in other groups, its
	hash's line and its notices, and moves its home if it is
	_/users/_*name*. Refused while any process acts for it, as
	_/processes_ shows. Only root may.

*set* _name_ [*full-name* _text_] [*admin* *yes*|*no*]
	Changes the account's full name, or whether it is an
	administrator's. Root changes either; anyone changes their own full
	name, as Linux's chfn lets them. Someone made no longer an
	administrator keeps nothing of the admin set: their elevated
	commands end and their panes lock.

*session* _name_ _password_ _term_
	Starts _name_'s session, if _password_ is theirs, on the terminal
	sent with the request as a descriptor (*SCM_RIGHTS*), with *TERM*
	set to _term_. Only root may, as *login*(1) runs. A wrong password
	waits as *check*'s does. The answer is *ok* _process_, the number
	of the session's first process, and the connection stays open: when
	that process ends, authd writes *ended* _status_, its wait status,
	and closes it. See SESSIONS.

*elevate* _mode_ _password-kind_ _password_ _term_ _directory_ [_language_...] *--* [_command_...]
	Runs _command_ as an administrator, or root's shell for the
	_mode_ *shell* or *login* (*command* runs the command), in
	_directory_, for an administrator, after their password unless
	their pane is unlocked: _password-kind_ is *password*, with the
	password after it, or *none* or *ask*, with an empty field; *ask*
	lets consoled ask for it on a console (PROMPTS). Each
	_language_ is a setting *NAME=value* to keep. The request comes
	with four descriptors: the command's standard input, output and
	error, and the read end of a pipe. The answer is *ok password*
	when a password is needed and none came, or *ok* _process_, after
	which the connection stays open as a session's. See ELEVATE.

*unlock* _password-kind_ _password_
	Unlocks the asker's pane for 15 minutes, or 15 more, after the
	password unless it is unlocked: *ok*, or *ok password*.

*forget*
	Locks the asker's pane again. Anyone may ask.

*prompts*
	Asks for the passwords to be asked on the consoles: *ok*, then the
	connection stays open for *ask* and *done* lines (PROMPTS). Only
	root may, as *consoled*(8) runs; a later one takes over.

*prompted* _number_ *password*|*cancelled*|*elsewhere* _password_
	What came of prompt _number_: the password typed, a prompt closed,
	or a terminal that is not a console's, with an empty _password_ for
	the last two. Only the process that asked *prompts* may.

authd knows who asks by the connection: the kernel gives it the process
at the other end, and the user and group it acted for when it connected
(*SO_PEERCRED*, *radix-posix*(7)). An administrator is root inside
*elevate*(8): see ELEVATE.

# WRONG PASSWORDS

A wrong password is answered only after a wait: 2 seconds for each of
the first three in a row, then twice as long each time, up to a minute,
for that account asked by that user. A try made while one waits waits
its turn, so tries at once do not go faster. A right password ends the
run; one forgotten after 15 minutes with no try ends it too. A name
there is not is checked against a hash all the same, and answered the
same way after the same wait, so that neither the answer nor the time
says whether the account is there.

authd answers 64 connections at a time and 4 from one process; one that
sends no whole request in 10 seconds is closed.

# SESSIONS

A session is the processes of someone who signed in, acting for their
user and groups (*radix-names*(2)), on the terminal they signed in at
(ADR 0250). authd starts its first process itself, as its own child,
with the *names* capability that Spark gives it because its service
file asks for it (*names = true*, *service.toml*(5)), and that no other
service holds:

- it stamps a copy of its root for the user and their groups, and makes
  one without the bypass right or the control right from that, the
  session's root, so that the session mounts and unmounts nothing;
- it opens _/system/commands/login_ through that root, so that it is a
  program the user may run;
- unless the user is root, it builds a view (ADR 0215) acting for them,
  as the session's root in place of that copy: each name of the root at
  its place, with the user's own _/tmp_ at _tmp_ (see below);
- it makes a process, names it for the user and their groups, and gives
  it that root, the random source, the log, the terminal as its
  descriptors 0, 1 and 2, and the network (see below), and nothing else
  of authd's: not its root, not the names capability, not the bypass
  right;
- it runs *login --session* in it, with the environment *HOME*,
  *SHELL*, *USER*, *LOGNAME*, *PATH=/system/commands*, *TERM*,
  *LANG=C.UTF-8* and *EDITOR=nano*; login makes the terminal the
  controlling terminal of a new session and runs the user's shell in
  their home.

Each person has a _/tmp_ of their own (ADR 0255),
_/system/live/authd/tmp/_*number*, theirs and mode 1777, which authd
makes at their first sign-in; their sessions share it, so tmux and
screen find their servers from any of them, and no one else's view
holds it. It lasts until the computer stops, and removing the account
removes it. Each place of the view is attached without the bypass
right and without control, so the owners and modes of the files decide
as they would without it, and nothing is mounted or attached in it. A
name added to the root after a session started is not in its view
until the next sign-in. Root's sessions, and commands run with
*elevate*(8), keep the whole tree and the system's _/tmp_; root finds a
person's at its real path.

The network comes from the capabilities that authd's service file asks
Spark for (*network.outbound*, *network.listen* and *admin.network*,
ADR 0257). A session connects out, and listens on the ports from 1024,
as anyone may on Linux, but sets up no interface and listens on no port
below 1024: binding one fails with *EACCES*, as on Linux. Root's
sessions, and commands run with *elevate*(8), get the network as authd
holds it, so *ip*(8) sets the interfaces up and a server listens on
port 80.

The groups are the account's own and each group of
_/system/settings/group_ that lists its name, up to 32. A session
started, and a session ended, with the wait status of its first
process, are security events:

```
authd: alice signed in on pts/0: session process 63, for process 58 (user 0)
authd: alice signed out on pts/0: session process 63 ended with status 0x0
```

The sessions' processes run in authd's resource domain, as the shells
ran in *consoled*(8)'s before sign-in, so its service file sets no
memory limit. A process authd makes ends when authd does, so a restart
of authd ends the sessions it started.

Without the names capability authd logs *no names capability: sign-in
cannot start sessions* when it starts, and refuses *session*.

Before a session's programs start, authd writes on its terminal when
the person last signed in, and the wrong passwords typed for them since
(see SIGN-IN RECORDS), as Linux's pam_lastlog does:

```
Last sign-in: Sat Oct 31 12:40 2026 on pts/0
Since then, 1 wrong password was typed for bob, the last on Sat Oct 31 12:44 2026 on pts/1.
```

Then what the person is to be told since they last signed in, which
authd forgets once the session starts: today, that a password was reset
from the startup-trouble menu, which every administrator is told:

```
authd: the password of alice was reset from the startup-trouble menu, by process 61 (user 0)
```

```
Your password was reset from the startup-trouble menu on Sat Oct 31 12:40 2026.
The password of alice was reset from the startup-trouble menu on Sat Oct 31 12:40 2026.
```

# SIGN-IN RECORDS

authd records the sign-ins in _/system/state/sign-ins_, for *last*(1),
*lastb*(1) and *lastlog*(8) (ADR 0254), a line of text each: the time
in seconds since the epoch, what happened, and its fields, *-* for
none:

```
1793440000 boot 0.13.0
1793440100 in alice pts/0 - 61
1793440200 failed bob pts/1 -
1793440300 out alice pts/0 - 61
1793440400 down
```

- *boot*: the computer started, with the kernel's release. authd
  records it as it starts, once for each start of the computer, with
  the kernel's time of the start.
- *down*: authd was asked to end, as Spark asks each service when the
  computer stops. The sessions end with it.
- *in* and *out*: someone signed in, or their session's first process
  ended, with the terminal, where from (*-* at the computer itself),
  and that process, which pairs them.
- *failed*: a wrong password was typed at a sign-in, with the name
  typed.

The starts and stops are in _system_, which everyone reads. Each
account's sign-ins, sign-outs and wrong passwords are in a file named
by its number, root's and the account's own group's, mode 0640, so the
person and root read it. The wrong passwords typed for names that are
no account's are in _unknown_, root's alone. The directory is 0711, so
no one but root lists it. A file that grew past 512 KiB keeps its
newest 4096 lines, and its newest sign-in with its sign-out and its
newest start if they are older.

# ELEVATE

*elevate*(8), and *sudo*(8), which is the same program, run a command as
an administrator through authd (ADR 0219). For an *elevate* request
authd:

- finds the asker's account by the user the connection gives, and
  refuses one that is not an administrator's, a member of the *admins*
  group, saying so in the audit log, before any password;
- finds the command in _/system/commands_ and _/applications/commands_
  only, or as given if it holds a */*, and refuses one not found;
- unless the asker's pane is unlocked, answers *ok password* to a
  request without a password, and checks the one that comes, waiting
  after a wrong one as for *check*;
- makes a process, its own child, acting for root as authd does, and
  gives it authd's root, which passes the owners and modes of files
  (the bypass right), the random source, the log, the platform
  capability that its service file's *admin.hardware* gives it, its
  network capabilities, and the three descriptors as its 0, 1 and 2:
  the admin set;
- runs *elevate --started* in it, which makes the first terminal among
  them its controlling terminal in a new session, goes to the
  directory and runs the command, with the environment *elevate*(8)
  describes;
- answers *ok* _process_, unlocks the pane for 15 minutes from then,
  and audits it.

A pane is the terminal of the asker's session, with that session and
when its leader started, as *radix-proc*(5) shows them: another session
on the same terminal is another pane, and a program with no terminal
has none, so it is asked each time. Unlocked panes are kept in memory
only, and forgotten when authd starts again.

Then authd reads the pipe: a line *signal* _number_ sends the command's
process group that signal, and *continue* sends it *SIGCONT*; the end
of the pipe, or of the connection, hangs the command up with *SIGHUP*,
as elevate is gone. When the command stops, authd writes *stopped*
_signal_ on the connection; when it ends, *ended* _status_, and closes
it. The audit lines:

```
authd: alice elevated on pts/0 (password typed): process 81 runs /system/commands/id, for process 77
authd: alice elevated on pts/0 (unlocked): process 84 runs /system/commands/user add bob, for process 77
authd: alice's elevated process 84 on pts/0 ended with status 0x0
authd: alice unlocked elevate on pts/0 for 15 minutes, for process 77
authd: bob, not an administrator, asked to elevate on pts/1: dmesg, for process 93
```

A command line shows as at most 256 characters, with *?* for each
control character. An elevated command runs in authd's resource domain
and ends when authd does.

# PROMPTS

*consoled*(8) asks *prompts* when it starts, so that the password for
elevate, in a pane on a console, is asked where no program draws or
reads (ADR 0251). An *elevate* or *unlock* request whose
_password-kind_ is *ask*, from a locked pane, is then not answered *ok
password*: authd keeps it, with its descriptors, and writes on
consoled's connection

```
ask 12 pts/0 alice /system/commands/user add bob
```

the prompt's number, the pane's terminal, whose password it is and the
command line, empty for *unlock*. consoled answers with a *prompted*
request: authd takes the password typed as the one the request came
with, waiting after a wrong one as ever, and elevate asks again; a
closed prompt is answered *no no password was typed*, and *elsewhere*,
for a terminal that is not a console's, *ok password*, so that elevate
asks itself. A prompt not answered in 5 minutes is given up. When its
elevate goes away first, authd writes *done* _number_, and consoled
takes the prompt off. While no program asks *prompts*, and when it
goes away, *ask* is answered as *none* is.

# ACCOUNTS

An account's name is 1 to 32 lowercase letters, digits, *\_*, *-* and
*.*, starting with a letter or *\_*, as Debian's adduser takes them. It
gets the lowest number from 1000 that no user or group has and no
removed account had, a group of
its own of that name and number, the shell *nukesh*(1) and the home
_/users/_*name*, of mode 0700, with _documents_, _downloads_, _desktop_,
_development_, _music_, _pictures_, _videos_ and _applications_, of mode
0755, and _app-data_, of mode 0700, all the user's. An administrator's
account is also in the *admins* group, number 10, which authd makes if
it is not there.

When it starts, authd makes the home of each account that has none,
such as one *ob image --user* made in an image before its first boot,
whose files are all root's. On a machine with no account, whose image
was made without *--user*, the first sign-in makes the owner's
(*login*(1)). It makes the folders while the home is
still root's, and gives the home to its user last, so that a user
already running cannot put a link where a folder is to go.

Each account made, disabled, enabled, removed, renamed or changed, each
password changed and each wrong password is a security event in the
audit log (*logd*(8)), with the process and user that asked. A name a request gave is shown as at most 64 characters,
with *?* for each control character, so that it cannot start a line of
its own:

```
authd: made the account alice (1000), an administrator's, for process 41 (user 0)
authd: a wrong password for alice, from process 52 (user 0); 1 in a row
```

# FILES

_/system/settings/passwd_
	The users.

_/system/settings/group_
	The groups.

_/system/state/authd/passwords_
	The passwords' hashes, a disabled account's with *!* in front.

_/system/state/authd/retired_
	The numbers of removed accounts, which no account gets again: a
	line _number_:_name_:_date_ each.

_/system/state/authd/notices_
	What each person is told at their next sign-in: a line
	_name_<tab>_text_ each.

_/system/state/sign-ins_
	The sign-in records (see SIGN-IN RECORDS).

_/system/live/authd/socket_
	The socket authd answers at.

_/system/live/authd/tmp/_*number*
	Each person's own _/tmp_, which only root lists.

_/users/_*name*
	Each account's home.

# EXIT STATUS

As a service, authd does not exit but when it is stopped, with 0; it
exits with 1 if it cannot start.

# SEE ALSO

*login*(1), *elevate*(8), *sudo*(8), *user*(8), *passwd*(1), *id*(1), *last*(1),
*lastlog*(8), *consoled*(8), *service*(8),
*service.toml*(5), *spark*(8), *logd*(8), *radix-names*(2), *radix-posix*(7),
*systools*(1)
