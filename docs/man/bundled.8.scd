bundled(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

bundled - install, seal and remove bundles, and keep what they may do

# SYNOPSIS

*bundled*

# DESCRIPTION

bundled installs bundles, seals them so that nothing changes them, and
removes them, for whoever asks with *bundle*(1) (ADR 0223). It links the
commands of the bundles it installed where the shell finds them, and
tells *bundle run* which bundle to run and what it recorded of it (ADR
0225). It keeps what each person said of each bundle's permissions, and
asks them through the console's trusted prompt (ADR 0224). Spark runs it as the *bundled* service, after authd, whose
accounts say where each home is, and before the consoles. It acts for
root, past the owners and modes of files, since it seals each bundle as
root's in each person's home.

It answers on _/system/live/bundled/socket_, which anyone may connect to,
one request per connection, and knows who asks by the user the kernel
gives for the connection, never by what the request says. Each request
is a line of tab-separated words; *bundle*(1) is the only program meant
to speak it.

# PLACES

A bundle is installed for everyone, in _/applications/ID_, or for one
person, in _ID_ in the _applications_ folder of their home. Installing or
removing for everyone is an administrator's, through *elevate*(8); a
person installs and removes their own. bundled installs only in a place's
folder that is its owner's (root's for _/applications_), opens every name
there one part at a time and never through a link, so nothing a person
puts in their folder leads it elsewhere.

# INSTALLING

The archive comes with the request, as a descriptor. bundled makes a
folder of its own, _.installing-RANDOM_, mode 0700, in the place's
folder, and unpacks the archive into it with the confined unpacker of
*bundle*(1), which holds only the archive and that folder. Then it checks
the folder again itself: the signature, the list, every file's SHA-256,
and that it holds only folders and regular files.

It decides whether to trust the key. A key the ID is bound to is
trusted, and any other key for that ID is refused; the asker's developer
key is trusted for their own installs; any other key installs only
after a warning that this computer does not know it, and from then on
the ID is bound to it. A command named as one of _/system/commands_, or
as one another bundle in reach adds, is refused.

It answers with the install sheet: the name, ID, version and publisher,
the key and why it is trusted or not, the size, the permissions asked
for in the system's own words and how each is given, and the commands,
saying plainly which run with your access, outside the sandbox. bundle
asks, unless *--yes* answered. Then bundled seals the folder (each file
root's, mode 0444, or 0555 for a program; each folder root's, 0555),
renames it to its ID in place of the same ID from the same key, records
it, binds the ID to its key at its first install there, links its
commands, and writes a security event to the audit log. Anything that
fails removes the folder it made, and nothing is recorded.

# COMMANDS

Each place has a _commands_ folder of bundled's, root's, mode 0755:
_/applications/commands_ and each person's _applications/commands_. It
holds a symbolic link by each command of the place's bundles, to
_/system/commands/bundle_, which runs the command of the name it was
called by (*bundle*(1), RUNNING). A session's *PATH* names both folders
after _/system/commands_, so installing never edits *PATH* or a shell's
profile. bundled makes each folder again from what it recorded after
each install and removal, and at its start; a _commands_ there that is
not a folder of root's is not bundled's, and it links nothing in it.

# RUNNING

Asked which bundle to run, by its ID or by a command it adds, bundled
answers with the asker's own bundle before everyone's: its place, ID and
key, and the SHA-256 of the signed list it recorded at install. *bundle
run* checks the folder against them before it runs anything.

# GRANTS

bundled keeps what each person said of each bundle's permissions, the
grants: for each, the bundle's ID and its publisher's key, the
permission, allow or never, since when, what it gives (each folder, read
or read-write, or app) and what the bundle asked for then. *bundle run*
gets those of the asker's, for the bundle's key only, and decides what
the program gets (*bundle*(1), PERMISSIONS).

Asked *ask* about a permission the asker has not answered for good,
bundled checks that the bundle is the one installed, reads the
permission from its sealed manifest itself, and words the question in
the system's own words. It finds the terminal of the process that asks,
its session's, from _/processes_, and asks *authd*(8) to have
*consoled*(8) ask it on that console's bottom line, where no program
draws, reads or answers. *bundle grant* asks the same way, *Y* or *N*.
An answer for good is recorded, and every answer is a security event of
the audit log. A terminal that is not a console's is answered
*elsewhere*, and nothing is given. Taking a permission away needs no
question: bundled records it, and tells each run of the bundle that
watches, on the *watch* connection its runner keeps, at once.

# REMOVING

bundled removes the sealed folder and the record, and with *--purge* the
asker's _app-data/ID_ and what they said of its permissions too, and
unlinks its commands. The ID stays bound
to its key, so a later version from the same publisher picks kept data
up and one from anyone else is refused. At its start it forgets a
recorded bundle whose folder is gone, and removes the working folders
it left behind.

# FILES

_/system/state/bundled/installed_
	Each bundle installed: its place, ID, version, key, the SHA-256 of
	its signed list, and when.

_/system/state/bundled/bound_
	Each ID's key, for everyone and for each person, since its first
	install there.

_/system/state/bundled/developer-keys_
	Each person's developer key, which *bundle new* and *bundle key*
	tell bundled.

_/system/state/grants/UID_
	Each person's grants, by their user number, mode 0600. One bundled
	cannot read is set aside as _UID.bad_, and its person is asked
	again.

_/system/live/bundled/socket_
	Where it answers.

_/applications_, _~/applications_
	The bundles for everyone and each person's own, and their
	_commands_ folders.

# SEE ALSO

*bundle*(1), *manifest.json*(5), *authd*(8), *consoled*(8), *service*(8), *elevate*(8), *log*(1)
