bundle(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

bundle - make, check, sign and pack bundles

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

# DESCRIPTION

A bundle is an app, a command-line tool or a service in one folder,
which its _manifest.json_ describes (*manifest.json*(5)). bundle makes
and checks manifests with the same reader that installs bundles, so what
*bundle check* says is what installing will say. It signs a bundle,
packs it into one _.dkv_ file to share, and checks and unpacks one.

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

# FILES

_$XDG_DATA_HOME/bundle/developer.key_, _developer.pub_
	Your developer key, a minisign secret key without a password, mode
	0600, and its public half. _$XDG_DATA_HOME_ is _~/.local/share_
	unless set.

# EXIT STATUS

0 when the manifest reads, and 1 when it does not, a file it names is
missing, a bundle does not check, or the command line is wrong.

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
```

The same checks with other tools, on any system:

```
$ cd notes && minisign -Vm files.sha256 -x signature -P RWS...
$ sha256sum -c files.sha256
$ unzip -t org.example.notes-0.1.0.dkv
```

# SEE ALSO

*manifest.json*(5), *service.toml*(5), *sha256sum*(1), *zip*(1), *systools*(1)
