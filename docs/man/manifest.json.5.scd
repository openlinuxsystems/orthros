manifest.json(5) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

manifest.json - what a bundle is, what it runs and what it asks for

# SYNOPSIS

_folder/manifest.json_, at the top of a bundle's folder and of the
_.dkv_ file made from it

# DESCRIPTION

A bundle is an app, a command-line tool or a service, with everything it
needs, in one folder or one _.dkv_ file. Its _manifest.json_ says what
it is, which programs it holds, the commands and services it adds, the
permissions it asks for and the data it keeps (ADR 0221). Dukovo reads
nothing else to install it, show it, or decide what it may do.

The file is JSON (RFC 8259), without comments, read strictly by the same
code that installs bundles, so *bundle check* says what installing will
say (*bundle*(1)):

- A field this version does not know is an error, so a misspelt one is
  never silently ignored. The exceptions are fields whose names start
  with *x-*, which are a vendor's own and which Dukovo reads past, such
  as *x-com.example.build*, and *$schema*, which editors read.
- Each error names its line and says what is wrong in plain words:

```
nginx/manifest.json:12: program nginx has no path for aarch64
```

- A field written twice is an error.

*bundle new* _id_ writes a manifest to start from, and *bundle schema*
prints its JSON Schema, for an editor that checks as you type.

# VERSIONS

*manifest_version* = _1_
	The version of this format the manifest is written for; every
	manifest has it. Version 1 is a preview until Dukovo 1.0.0, when it
	freezes; after that a version only adds fields and never changes
	what one means, and a later Dukovo reads every earlier version. A
	manifest for a version newer than this Dukovo reads is refused,
	with a message to update Dukovo.

The names of what later versions bring are reserved, and a manifest
that uses one is refused: *entries* and *actions*, where an app shows up
on the desktop and what it can do there (Dukovo 1.5), *display*, its
windows (1.1), *runtime*, the Linux layer (1.3), and *plugins* (1.10).

# FIELDS

*id* = _"reverse.dns.name"_
	The bundle's ID: a domain its publisher holds, reversed, and the
	bundle's own name, lowercase, such as *"org.nginx.nginx"*. At
	least two parts of letters, digits and *-*. Installing ties it to
	the key that signs the bundle. Required.

*name* = _"text"_
	What people call it: one line, at most 200 characters. Required.

*summary* = _"text"_
	What it is, in one line. Required.

*description* = _"text"_
	More about it, in paragraphs, at most 4000 characters.

*version* = _"version"_
	Its version: numbers between dots, one to four of them, as
	semantic versioning writes them, such as *"1.27.3"* or *"17.2"*,
	with a pre-release after *-* (*"2.0.0-beta.1"*) and a build after
	*+*. Required.

*publisher* = { *name* = _"text"_, *url* = _"https://..."_ }
	Who publishes it, and their web site. The key comes from the
	signature, never from the manifest. *name* is required.

*license* = _"expression"_
	Its license, as an SPDX expression (https://spdx.org/licenses/):
	identifiers joined by *AND* and *OR*, an exception after *WITH*,
	and parentheses, such as *"MIT OR Apache-2.0"*, or
	*"LicenseRef-Proprietary"* for terms of the publisher's own.
	Required.

*source* = _"https://..."_
	Where the exact source of this version is. A bundle under a GNU
	license (GPL, LGPL or AGPL) must have it, which those licenses ask.

*homepage* = _"https://..."_
	Its web site.

*icon* = _"path"_
	Its icon: an SVG file in the bundle, such as
	*"resources/app.svg"*.

*requires_os* = _"version"_
	The first Dukovo release it runs on, such as *"1.0"* or
	*"1.4.1"*. Required.

*architectures* = [_"name"_, ...]
	The architectures it carries programs for, each once: *"x86_64"*;
	later *"wasm"* (Dukovo 1.9), *"aarch64"* (1.16) and *"riscv64"*
	(1.17). One bundle may carry several. Required.

*programs* = { _"name"_: { *path* = {...}, *permissions* = [...] } }
	Its programs, by name, at least one. *path* gives the file for
	each architecture, as a path in the bundle such as *"bin/nginx"*:
	one for each of *architectures*, and no other. *permissions*, if
	given, narrows the program to some of the bundle's permissions,
	each one the bundle asks for. Required.

*commands* = [ { *name*, *program*, *runs_with* } ]
	The commands it adds to the terminal: the name people type, the
	program it runs, and *runs_with*, either *"sandbox"*, with what the
	bundle's permissions give, or *"your-access"*, with everything the
	shell that starts it holds, as a program of the system's does. Each
	name once.

*services* = [ {...} ]
	The services it runs, each with the settings of a service file
	(*service.toml*(5)), its program by name:

	*name*
		Its name, which *service*(8) takes, written as a service
		file's name is. Required, and each once.

	*program*
		The program it runs, one of *programs*. Required.

	*arguments* = [_"word"_, ...]
		Its arguments, after the program's name.

	*environment* = { _"NAME"_: _"value"_ }
		More of its environment.

	*account* = _"name"_
		The account it runs as, its own, made when it is installed;
		the service's name unless given, which must then be one an
		account may have: lowercase letters, digits, *\_*, *-* and *.*.

	*restart* = *"always"* | *"on-failure"* | *"never"*
		When it is started again after it ends; *"always"* unless
		given.

	*limits* = { *cpu_weight*, *memory*, *oom*, *open_files* }
		What it may use: a service file's limits, and *open_files*,
		how many files each of its processes may have open, 1 to
		1048576 (Linux's *fs.nr_open*), 1024 unless given.

	*secrets* = [_"name"_, ...]
		The names of the secrets it gets as read-only files, as a
		service file's.

	*start* = *"when-enabled"* | *"at-boot"* | *"on-connection"*
		When it starts: once an administrator enables it (the
		default), at boot once it is installed, or at the first
		connection to a socket it provides, which *provides* names.

	*needs* = [_"interface/1"_, ...]
		The interfaces that must be up before it starts.

	*health* = { *socket* = _"name"_ }
		How to tell it works: a socket it provides, which answers once
		it does.

*permissions* = { _"name"_: { *need*, *why*, ... } }
	The permissions it asks for, by the names of the list below, each
	once. *need* is *"required"*, when the bundle does not work without
	it and is not installed without it, or *"optional"*, when it works
	with less. *why* is one line in its own words, shown under the
	system's, which an app cannot reword; *bundle check* notes each
	permission without one. A permission that takes more takes it here:

	*folders* = { _"name"_: *"read"* | *"read-write"* }
		For *files.folder*, your folders by name; for *files.shared*,
		folders of _/shared_ by name.

	*ports* = [_port_ | _"first-last"_, ...]
		For *network.listen*: ports and ranges of ports, not
		overlapping, such as *[80, 443]*.

	*apps* = [_"id"_, ...]
		For *files.app-data*: the IDs of the other bundles.

*data* = { *settings*, *data*, *cache* }
	Which of its folders it keeps, each *true* or *false*: settings and
	data, which backups keep, and a cache, which is never backed up or
	snapshotted.

*provides* = [ { *socket* | *interface*, *to* } ]
	The sockets and interfaces it offers, each a socket by name or an
	interface by name and major version (*"org.example.sync/1"*), one
	of the two, and *to*, who may connect: *"approved"*, the programs an
	administrator or you approve; *"anyone"*; *"publisher"*, the
	publisher's other bundles; or *"system"*, the system's own programs.

*uses* = [ { *interface*, *need* } ]
	The interfaces of other bundles and of the system it talks to, such
	as *"dukovo.certificates/1"*, each *"required"* or *"optional"*.

*update* = { *feed* = _"https://..."_, *key* = _"minisign key"_ }
	Its own update feed, for a bundle outside a catalog, and the
	minisign public key that signs it, as *minisign -G* writes it.

*translations* = { _"language"_: { *name*, *summary*, *description*, *why* } }
	Its texts in other languages, by BCP 47 tag such as *"de"* or
	*"pt-BR"*; *why* gives the reason for each permission it asks for.

# PERMISSIONS

Each permission has the words the system shows for it and a way it is
given. Version 1 defines these:

[[ *Permission*
:- *What the system says*
:- *Given*
|  *files.folder*
:  Use these folders of yours (*folders*: documents, downloads, desktop,
   development, music, pictures, videos)
:  on first use
|  *files.volumes*
:  Use drives you plug in
:  on first use
|  *files.shared*
:  Use these folders of /shared (*folders*)
:  on first use
|  *files.home*
:  Use all of your files
:  with a warning
|  *files.app-data*
:  Read these other apps' private data (*apps*)
:  with a warning
|  *network.outbound*
:  Connect to the internet and to services on this computer
:  at install
|  *network.listen*
:  Accept connections on these ports (*ports*)
:  at install
|  *network.local*
:  Reach devices on your local network and see what discovery finds
:  on first use
|  *network.raw*
:  Send raw packets and capture network traffic
:  by an administrator
|  *devices.serial*
:  Use a serial port or USB serial device
:  on first use
|  *start-at-login*
:  Start when you sign in
:  at install
|  *notifications*
:  Show notifications
:  always, unless you turn it off
|  *processes.debug*
:  Trace or debug your other programs
:  with a warning
|  *runs-with-your-access*
:  Run with everything you can reach, without a sandbox
:  with a warning
|  *admin.users*, *admin.services*, *admin.network*, *admin.firewall*,
   *admin.storage*, *admin.updates*, *admin.apps*, *admin.logs*,
   *admin.settings*, *admin.power*, *admin.printers*, *admin.hardware*,
   *admin.users-files*
:  Administer accounts, services, the network, the firewall, disks,
   updates, apps for everyone, read the logs, change the system's
   settings, shut down, administer printers, drive fans and lights, and
   reach every person's files
:  by an administrator

These are reserved, and defined in the manifest version of the release
that brings them: *devices.usb* (1.6), *devices.camera* (1.6.6),
*devices.microphone* (1.7), *devices.bluetooth* (1.6.5),
*devices.scanner* (1.7.1), *devices.gpu-compute* (1.6),
*devices.hardware* (1.8), *display* (1.1), *screen.capture* (1.5.2),
*input.control* (1.5), *shortcuts.global* (1.5), *clipboard.read* (1.1),
*background* (1.1), *location* (1.6), *contacts*, *calendar*,
*reminders* and *mail* (1.5.6), *keychain.shared* (1.4.1), *ai.ask*
(1.10) and *memory.jit* (1.2).

Dukovo may later narrow what a permission gives, or ask again, when that
protects people: a bundle keeps installing and running, perhaps with
less.

# EXAMPLE

nginx, as the catalog will ship it:

```
{
  "manifest_version": 1,
  "id": "org.nginx.nginx",
  "name": "nginx",
  "version": "1.27.3",
  "summary": "Web server and reverse proxy",
  "publisher": { "name": "Dukovo catalog", "url": "https://dukovo.org" },
  "license": "BSD-2-Clause",
  "requires_os": "1.0",
  "architectures": ["x86_64"],
  "programs": { "nginx": { "path": { "x86_64": "bin/nginx" } } },
  "commands": [ { "name": "nginx", "program": "nginx", "runs_with": "your-access" } ],
  "services": [ {
    "name": "nginx",
    "program": "nginx",
    "arguments": ["-g", "daemon off;"],
    "account": "nginx",
    "restart": "on-failure",
    "limits": { "memory": "512M", "open_files": 65536 },
    "start": "when-enabled"
  } ],
  "permissions": {
    "network.listen": { "ports": [80, 443], "need": "required", "why": "Serves websites" },
    "network.outbound": { "need": "optional", "why": "Passes requests on to other servers" },
    "files.shared": { "folders": { "www": "read" }, "need": "required", "why": "Reads the sites it serves" }
  },
  "uses": [ { "interface": "dukovo.certificates/1", "need": "optional" } ],
  "data": { "settings": true, "cache": true }
}
```

# SEE ALSO

*bundle*(1), *service.toml*(5), *service*(8)
