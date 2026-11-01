bundle(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

bundle - make, check, sign, pack, install and run bundles, and say what they may do

# SYNOPSIS

*bundle new* _id_ [_folder_] [*--name* _name_] [*--publisher* _name_] [*--license* _expression_]

*bundle check* [_folder_ | _file_]

*bundle schema*

*bundle upgrade-manifest* [_folder_ | _file_]

*bundle key*

*bundle sign* [_folder_] [*--key* _secret-key-file_]

*bundle pack* [_folder_] [*--key* _secret-key-file_] [*-o* _file_]

*bundle verify* [_folder_ | _file_*.dkv*] [*--key* _public-key_]

*bundle unpack* _file_*.dkv* _folder_

*bundle install* _file_*.dkv* [*--for-everyone*] [*--yes*]

*bundle list*

*bundle info* _id_

*bundle remove* _id_ [*--for-everyone*] [*--keep-data* | *--purge*]

*bundle run* [*--command* _name_] _id_ [_argument_...]

*bundle grant* _id_ _permission_ [_folder_*=read* | _folder_*=read-write* | _app-id_]...

*bundle revoke* _id_ _permission_

# DESCRIPTION

A bundle is an app, a command-line tool or a service in one folder,
which its _manifest.json_ describes (*manifest.json*(5)). bundle makes
and checks manifests with the same reader that installs bundles, so what
*bundle check* says is what installing will say. It signs a bundle,
packs it into one _.dkv_ file to share, and checks and unpacks one. It
installs, lists and removes bundles through *bundled*(8), as apt and
dpkg do packages, runs them, each in a sandbox of its own, and gives
them the permissions you allow.

A signed bundle has _files.sha256_, the SHA-256 of each of its other
files as *sha256sum*(1) writes them, and _signature_, minisign's Ed25519
signature of that list, whose first line names the key that made it. The
signature thus covers every file, and the bundle checks the same way as a
folder and as a _.dkv_ file, which is a zip of the folder.

*new* _id_ [_folder_]
	Makes _folder_, or a folder named for the last part of _id_, with
	a _bin_ folder and a _manifest.json_ for a bundle of one program and
	its command, which runs in the bundle's sandbox and asks for no
	permission. _id_ is reverse DNS, lowercase, such as
	*org.example.notes*. *--name* gives what people call it (the last
	part of _id_ unless given), *--publisher* who publishes it (your
	user name unless given), and *--license* its license as an SPDX
	expression, *LicenseRef-Proprietary*, your own terms, unless given.
	It writes over no manifest that is there already.

*check* [_folder_ | _file_]
	Checks the manifest of the bundle in _folder_, the current folder
	unless given, or the manifest _file_. Each error is a line of its
	own on standard error, as _file_*:*_line_*:* _why_; a note, such as
	a permission asked for without a *why*, goes to standard output.
	Given a folder, it also checks that each program the manifest names
	is there, for each architecture, and may be run, and that its icon
	is there. It prints _file_*: ok:* _id_ _version_ when all is well,
	and exits with 1 when anything is wrong.

*schema*
	Prints the JSON Schema (draft 2020-12) of the manifest, made from
	the types the reader uses, for an editor that checks a manifest as
	it is written.

*upgrade-manifest* [_folder_ | _file_]
	Brings a manifest of an earlier version up to the newest this
	Dukovo reads. Version 1 is the first, so for now it checks the
	manifest and says it is the newest.

*key*
	Prints your developer key, a minisign public key (*RW* and 54 more
	characters), making the key first if you have none. The developer
	key signs your own builds; *bundle new* makes it the first time.

*sign* [_folder_] [*--key* _secret-key-file_]
	Checks the bundle in _folder_, the current folder unless given, as
	*check* does, then writes its SBOM, _sbom.spdx_, if it has none,
	the list of its files, _files.sha256_, and the signature of the list,
	_signature_. It signs with your developer key, or with the minisign
	secret key _secret-key-file_, which must have no password
	(*minisign -G -W* makes one). An SBOM written here lists the bundle
	alone; list the libraries the bundle carries in your own, so that
	their flaws are found.

*pack* [_folder_] [*--key* _secret-key-file_] [*-o*, *--output* _file_]
	Packs the bundle in _folder_ into _id_*-*_version_*.dkv* in the
	current folder, or _file_. A folder whose signature checks is packed
	as it is; one that is not signed or has changed since is signed
	first, as *sign* does, and *--key* signs it again with that key. The
	same folder packs to the same bytes.

*verify* [_folder_ | _file_*.dkv*] [*--key* _public-key_]
	Checks a bundle's signature and every file against it: no file more
	or less than its list holds, and each as it was signed. It prints
	_what_*: ok:* _id_ _version_, the files, and whose key signed it and
	when, or why it does not check, and exits with 1. With *--key*, a
	public key (*RW*...) or a minisign public key file, it also fails
	unless that key signed the bundle.

*unpack* _file_*.dkv* _folder_
	Checks the archive and unpacks it into _folder_, which it makes and
	which must not be there. The archive is read by a process of its own
	that holds the archive, the new folder and a pipe for its report, and
	nothing else, not even a root: a path in the archive cannot lead out
	of the folder. It refuses encryption, zip64, links, device files,
	set-ID bits, names outside the folder or twice, files that unpack to
	more than 100 times the archive's size or more than the disk has
	free, and any file not as it was signed; then it removes the folder.
	A program the manifest names may be run once unpacked, as may a file
	the archive marks so.

*install* _file_*.dkv* [*--for-everyone*] [*--yes*]
	Installs the bundle for you, in your _applications_ folder, or with
	*--for-everyone*, which only an administrator may, through
	*elevate*(8), in _/applications_. *bundled*(8) unpacks and checks it,
	and shows the install sheet: what it is, whose key signed it and
	whether this computer knows the key, its size, the permissions it
	asks for in the system's own words, and its commands. bundle asks
	*Install it?*; *--yes* answers for scripts. Installed, the bundle is
	sealed: root's and read-only, so nothing changes it. A bundle signed
	by your developer key installs with no warning for you; any other key
	this computer does not know installs after a warning, and from then
	on only that key may install that ID.

*list*
	Lists the bundles you can run, yours and everyone's: ID, version,
	whose, and name.

*info* _id_
	Shows an installed bundle's sheet, what you said of its permissions
	and since when, when it was installed, where, and where its data is.

*remove* _id_ [*--for-everyone*] [*--keep-data* | *--purge*]
	Removes your bundle, or with *--for-everyone* one installed for
	everyone, through *elevate*(8). It asks whether to delete its
	settings and data, _~/app-data/id_, too; *--keep-data* and *--purge*
	answer for scripts. Kept data is picked up by a later version from
	the same publisher; what you said of its permissions goes with its
	data and stays with kept data.

*run* [*--command* _name_] _id_ [_argument_...]
	Runs the bundle's command _name_, or its first command, or with no
	command its only program, with each _argument_, options included;
	see RUNNING.

*grant* _id_ _permission_ [_folder_*=read* | _folder_*=read-write* | _app-id_]...
	Gives the bundle _permission_ from now on, as its manifest asks for
	it, or with less: *documents=read* for its *documents=read-write*,
	or some of the folders or apps it names. bundled asks you first, on
	the console's bottom line, *Y* or *N*, so that no program can give
	itself a permission by running bundle; on a terminal that is not a
	console's it refuses. It gives only what the bundle asks for, and
	nothing an administrator gives.

*revoke* _id_ _permission_
	Takes _permission_ away from the bundle, now and from now on,
	without a question; a run of it that holds it stops at once (see
	PERMISSIONS). *bundle grant* gives it back.

# RUNNING

A bundle's commands are on your *PATH*: *bundled*(8) links each, by its
name, in _/applications/commands_ or your _applications/commands_, to
bundle, which run by a name that is none of its own runs the bundle
command of that name. *notes* and *bundle run org.example.notes* run the
same program.

Before anything runs, bundle asks bundled which bundle it is and what it
recorded at install, and checks the bundle's folder against that: still
sealed (root's, mode 0555), its signed list the one recorded and signed
by the key recorded, its manifest the one the list names, and the
program a sealed file the list names. A bundle that fails any check does
not run, and bundle says why.

A command whose manifest says it runs with your access runs as a program
of the system's, with all you hold, once you allowed
*runs-with-your-access*, asked with a warning the first time it runs;
the install sheet says so plainly too. Any other runs in the bundle's
sandbox (ADR 0225), a process whose root is a view of:

- _/system/commands_, _libraries_, _resources_ and _settings_,
  _/system/live/notify_, _/processes_ and _/kernel_, read-only;
- _/devices/null_, _zero_, _random_ and _urandom_;
- the bundle's own folder, read-only, at its path;
- its data folder, _~/app-data/id_, which it may change, made if it
  is not there, mode 0700, and its home;
- a _/tmp_ of its own, which is _/tmp/.app-id_ of yours;
- the links of _/_ and _/system_, so _/bin/sh_ is there;
- what you allowed it (PERMISSIONS).

Nothing else of your home, nor of anyone's, is in it. It holds the
network only as its manifest asks: with *network.outbound*, to connect
out; with *network.listen*, to listen on those ports, out of those you
may (from 1024 for anyone but an administrator); and without either, no
network at all, so it cannot make a socket. Its environment is clean:
*HOME*, *USER*, *LOGNAME*, *PATH*=_/system/commands_, *TERM*, and your
*LANG*, *LC\_\** and *TZ*. Its descriptors 0 to 2 are yours; bundle
waits for it and exits with its status.

# PERMISSIONS

What else a program gets is what you allow (ADR 0224), of the
permissions its bundle asks for, as many as the program keeps
(*programs.*_name_*.permissions*). One given at install
(*network.outbound*, *network.listen*, *start-at-login*) or always
(*notifications*) is given unless you took it away. One asked on first
use or with a warning is asked the first time the bundle runs wanting
it, on the console's bottom line, which *consoled*(8) draws and no
program can draw, read or answer:

```
 [permission] Notes (org.example.notes) asks to: use your documents folder, to read and change it      O: once    A: always    D: don't allow
```

*O* allows it this time, *A* from now on, and *D* not, from now on;
*Esc* or *Ctrl+C* close the question, which leaves it out this time.
One that reaches what other apps keep, such as all of your files, says
*[warning]*, in white on red. On a narrow console the words are cut,
never the keys. A program that requires a permission you did not allow
does not run, saying how to give it; one that asks for it as optional
runs without it. Only the consoles ask: on another terminal, give it
with *bundle grant* at a console first.

Given, a permission is part of the program's view, each place at its
real path:

- *files.folder*: the folders of yours it names, such as
  _~/documents_, read-only or not as given, each made if it is not
  there;
- *files.home*: all of your home, which holds its data folder;
- *files.volumes*: _/volumes_, the drives you plug in;
- *files.shared*: the folders of _/shared_ it names;
- *files.app-data*: the data folders of the other apps it names, to
  read;
- *devices.serial*: the serial ports' terminals there are when it
  starts, _/devices/ttyACM_*N* and _/devices/ttyUSB_*N*, those you can
  open.

*network.local* and *processes.debug* cannot be given yet, nor any
permission an administrator gives: a program that requires one does not
run. A later version that asks for more than you allowed is asked again;
one that asks for the same or less is not. What you said holds only for
the publisher's key it was said of.

Taking a permission away with *bundle revoke* reaches each run of the
bundle that holds it at once. A view keeps what is attached to it and a
socket outlives the capability it was made with, so the program and
what it started are stopped, *SIGTERM* and two seconds later *SIGKILL*,
and bundle exits with 143; its next run goes without.

# FILES

_$XDG_DATA_HOME/bundle/developer.key_, _developer.pub_
	Your developer key, a minisign secret key without a password, mode
	0600, and its public half. _$XDG_DATA_HOME_ is _~/.local/share_
	unless set.

_/applications_, _~/applications_
	The bundles for everyone and your own, sealed, and their
	_commands_ folders.

_~/app-data/id_
	A bundle's settings and data, its home when it runs.

_/system/state/grants/uid_
	What each person said of each bundle's permissions, which only
	*bundled*(8) writes.

# EXIT STATUS

0 when the manifest reads, and 1 when it does not, a file it names is
missing, a bundle does not check, or the command line is wrong. *install*
and *remove* exit with 1 when bundled refused, and *grant* and *revoke*
when it refused or you said no. *run* and a bundle's
command exit with the program's status, 128 and the signal's number for
one a signal ended, or 1 when it did not run.

# EXAMPLES

```
$ bundle new org.example.notes
wrote notes/manifest.json
put the program in notes/bin/notes, then check the bundle with `bundle check notes`
$ bundle check notes
notes/manifest.json: program notes for x86_64 is not at bin/notes
$ bundle pack notes
notes: signed org.example.notes 0.1.0, 3 files, with your developer key RWS...
wrote org.example.notes-0.1.0.dkv: org.example.notes 0.1.0, 3 files, 1.7K, signed by your developer key RWS...
$ bundle unpack org.example.notes-0.1.0.dkv /tmp/notes
/tmp/notes: unpacked org.example.notes 0.1.0, 3 files, signed by your developer key RWS...
$ bundle install org.example.notes-0.1.0.dkv
notes 0.1.0, for yourself
  org.example.notes, by alice
  ...
Install it? [y/N] y
installed org.example.notes 0.1.0, for you
$ notes
```

Its first run asks about the permissions it asks for on first use, on
the bottom line. To give it your documents to read only, ahead:

```
$ bundle grant org.example.notes files.folder documents=read
gave org.example.notes files.folder (documents=read)
$ bundle revoke org.example.notes files.folder
took files.folder away from org.example.notes
```

The same checks with other tools, on any system:

```
$ cd notes && minisign -Vm files.sha256 -x signature -P RWS...
$ sha256sum -c files.sha256
$ unzip -t org.example.notes-0.1.0.dkv
```

# SEE ALSO

*bundled*(8), *manifest.json*(5), *service.toml*(5), *consoled*(8), *authd*(8), *elevate*(8), *sha256sum*(1), *zip*(1), *systools*(1)
