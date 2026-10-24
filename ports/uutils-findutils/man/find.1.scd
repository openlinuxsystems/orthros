find(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

find - search for files in a directory tree

# SYNOPSIS

*find* [*-H* | *-L* | *-P*] [*-O*{_level_}] [_path_ ...] [_expression_]

# DESCRIPTION

*find* walks the tree below each _path_ (the working directory if none is
given) and evaluates _expression_ for every file it meets, from left to
right, as the operators below say. With no action in the expression, it
prints the path of every file for which the expression is true.

Dukovo's *find* is the one of uutils findutils, in Rust, which follows GNU
find. This page describes it as Dukovo ships it.

# OPTIONS

These come before the paths.

*-P*
	Never follow symbolic links: a link is looked at itself. This is the
	default.

*-L*
	Follow symbolic links: a test looks at what a link points to, and a
	link to a directory is walked into.

*-H*
	Follow symbolic links named on the command line, and no others.

*-O*{_level_}
	Accepted for GNU find's sake, with no space before the level,
	which is ignored.

# EXPRESSIONS

An expression is made of tests, actions, positional options and
operators. Numbers _N_ in tests can be written as *+*_N_ (more than _N_),
*-*_N_ (less than _N_) or _N_ (exactly _N_).

## Operators

From the one that binds tightest:

*(* _expr_ *)*
	Grouping. The parentheses are separate arguments, and must be quoted
	from the shell: *\\(* and *\\)*.

*!* _expr_, *-not* _expr_
	True if _expr_ is false.

_expr1_ _expr2_, _expr1_ *-a* _expr2_, _expr1_ *-and* _expr2_
	True if both are; _expr2_ is not evaluated when _expr1_ is false.

_expr1_ *-o* _expr2_, _expr1_ *-or* _expr2_
	True if either is; _expr2_ is not evaluated when _expr1_ is true.

_expr1_ *,* _expr2_
	Evaluates both; the value is _expr2_'s.

## Positional options

These hold for the whole walk, wherever they are written, and are always
true.

*-daystart*
	Measure the times of *-amin*, *-atime*, *-cmin*, *-ctime*, *-mmin* and
	*-mtime* from the start of today rather than from now.

*-follow*
	The same as *-L*.

*-maxdepth* _N_
	Descend at most _N_ levels below the starting points; *-maxdepth 0*
	looks at the starting points only.

*-mindepth* _N_
	Apply no tests or actions at levels less than _N_.

*-depth*, *-d*
	Process a directory's contents before the directory itself.

*-mount*, *-xdev*
	Do not descend into directories on other filesystems.

*-noleaf*
	Accepted for GNU find's sake.

*-sorted*
	Walk each directory's entries in the order of their names (not in
	GNU find).

*-regextype* _type_
	The syntax of *-regex* and *-iregex*: *emacs* (the default), *grep*,
	*posix-basic* or *posix-extended*.

## Tests

*-name* _pattern_, *-iname* _pattern_
	The file's name (its path's last component) matches the shell pattern
	_pattern_ (*\**, *?*, *[...]*); *-iname* ignores case.

*-path* _pattern_, *-ipath* _pattern_, *-wholename* _pattern_, *-iwholename* _pattern_
	The whole path, as *find* prints it, matches _pattern_; *\** matches
	*/* too.

*-lname* _pattern_, *-ilname* _pattern_
	The file is a symbolic link whose target matches _pattern_.

*-regex* _regex_, *-iregex* _regex_
	The whole path matches the regular expression _regex_, in the syntax
	*-regextype* chose.

*-type* _c_
	The file is of type _c_: *b* (block device), *c* (character device),
	*d* (directory), *f* (regular file), *l* (symbolic link), *p* (pipe)
	or *s* (socket). Several types may be given separated by commas, as
	*-type f,l*.

*-xtype* _c_
	Like *-type*, but for a symbolic link, the type of what it points to
	(or with *-L*, of the link itself).

*-fstype* _type_
	The file is on a filesystem of type _type_: *tmpfs*, *fat*, *bootfs*,
	*proc*, *sys* or *dev* on Dukovo.

*-size* _N_[_unit_]
	The file takes _N_ units, rounded up: *b* for 512-byte blocks (the
	default), *c* for bytes, *w* for two-byte words, *k*, *M* and *G* for
	kibibytes, mebibytes and gibibytes.

*-empty*
	The file is an empty regular file or an empty directory.

*-newer* _file_, *-anewer* _file_, *-cnewer* _file_
	The file was modified (*-newer*), accessed (*-anewer*) or changed
	(*-cnewer*) more recently than _file_ was modified.

*-newer*_XY_ _reference_
	The file's time _X_ is newer than _reference_'s time _Y_, each one of
	*a* (access), *B* (birth), *c* (change) and *m* (modification); _Y_
	may also be *t*, when _reference_ is a time and date rather than a
	file.

*-amin* _N_, *-cmin* _N_, *-mmin* _N_
	The file was accessed, changed or modified _N_ minutes ago.

*-atime* _N_, *-ctime* _N_, *-mtime* _N_
	The file was accessed, changed or modified _N_ days (periods of 24
	hours) ago, the fraction left out: *-mtime 0* is within the last day.

*-perm* _mode_, *-perm -*_mode_, *-perm /*_mode_
	The file's permission bits are exactly _mode_; all of _mode_'s are
	set (*-*); any of _mode_'s is set (*/*). _mode_ is octal or symbolic,
	as *chmod*(1) takes it.

*-readable*, *-writable*, *-executable*
	The caller may read, write or run (for a directory, search) the file.

*-samefile* _file_
	The file is _file_: the same file number on the same filesystem.

*-user* _name_, *-uid* _N_, *-nouser*
	The file is owned by user _name_ (or number), by user number _N_, or
	by a number no user has.

*-group* _name_, *-gid* _N_, *-nogroup*
	The same for the file's group.

*-inum* _N_
	The file's number (*st_ino*) is _N_.

*-links* _N_
	The file has _N_ hard links.

*-true*, *-false*
	Always true; always false.

## Actions

*-print*
	Print the path and a newline. The default action.

*-print0*
	Print the path and a NUL byte, for *xargs -0*.

*-printf* _format_
	Print _format_, in which *\\n*, *\\t* and the other C escapes and
	these directives stand for the file's details: *%p* the path, *%P*
	the path without its starting point, *%f* the name, *%h* the
	directory, *%H* the starting point, *%d* the depth, *%s* the size in
	bytes, *%b* and *%k* the 512-byte and 1024-byte blocks it takes, *%S*
	its sparseness, *%m* and *%M* its permissions in octal and as *ls*(1)
	shows them, *%n* its hard links, *%i* its file number, *%D* its
	device, *%F* its filesystem's type, *%u* and *%U* its owner's name
	and number, *%g* and *%G* its group's, *%l* a link's target, *%y* and
	*%Y* its type and what it points to, *%a*, *%c* and *%t* its access,
	change and modification times as *ctime*(3) writes them, and *%A*_k_,
	*%C*_k_ and *%T*_k_ those times formatted by _k_, a *strftime*(3)
	letter or *@* for seconds since 1970. *%%* is a *%*. Field widths and
	flags work as in *printf*(3).

*-fprint* _file_, *-fprint0* _file_, *-fprintf* _file_ _format_
	As *-print*, *-print0* and *-printf*, into _file_, which is made or
	emptied first.

*-ls*, *-fls* _file_
	List the file as *ls -dils* does, on standard output or into _file_.

*-exec* _command_ *;*
	Run _command_; true if it exits with 0. A *{}* in it is replaced by
	the path. The *;* must be quoted from the shell.

*-exec* _command_ *{} +*
	Run _command_ with as many paths as fit at its end, as *xargs*(1)
	would; always true.

*-execdir* _command_ *;*, *-execdir* _command_ *{} +*
	Like *-exec*, run in the file's directory with *./*_name_ for *{}*.

*-ok* _command_ *;*, *-okdir* _command_ *;*
	Like *-exec* and *-execdir*, after asking on standard error and
	reading a yes from standard input.

*-delete*
	Remove the file, or the empty directory; implies *-depth*. True if it
	was removed.

*-prune*
	Do not descend into the directory; always true. Has no effect with
	*-depth*.

*-quit*
	Stop at once, after the commands *-exec ... +* has gathered have run.

# EXIT STATUS

0 if every file was processed, 1 if an error happened (a directory that
could not be read, say), as GNU find does.

# EXAMPLES

Files under _/system/var/log_ changed in the last day:

	find /system/var/log -type f -mtime 0

Remove the build output of every project, but nothing below it:

	find . -name target -type d -prune -exec rm -r {} +

Names with spaces and all, handed to another program:

	find . -name '\*.txt' -print0 | xargs -0 wc -l

# SEE ALSO

*xargs*(1), *ls*(1), *chmod*(1), *stat*(1), *radix-files*(7)

# COPYRIGHT

find is part of uutils findutils, https://github.com/uutils/findutils, by
the uutils developers, under the MIT license. Dukovo builds it from
release 0.9.1 (ports/uutils-findutils). This page is Dukovo's.
