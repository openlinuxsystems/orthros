service(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

service - start, stop and look at the services, and check their files

# SYNOPSIS

*service* _name_ *start* | *stop* | *restart* | *--full-restart* | *reload* | *status*

*service --status-all*

*service check* [_name_ | _file_]...

*service new* _name_

*service edit* _name_

*service secret* *set* _name_ _secret_ | *list* _name_ | *remove* _name_ _secret_

# DESCRIPTION

service asks Spark, over its control socket _/system/live/spark/control_
(*spark*(8)), about the services it supervises: the programs that
_/system/settings/spark/services_ describes, a file each.

*start* starts a service that is stopped or disabled, *stop* stops one
and keeps it stopped until it is started again or the system restarts,
and *restart* stops it and starts it again; Spark stops the domain of
one that has not ended 5 seconds after SIGTERM. *reload* sends a running
service SIGHUP, which has consoled read its keyboard layout's setting
again (*consoled*(8)). *status* shows what Spark knows of it:

```
consoled - Virtual consoles
   Active: active (running) as process 14
  Restart: always, 0 restarts so far
```

*--status-all* lists every service, *[ + ]* for one that runs, *[ - ]*
for one that does not, and *[ ? ]* for one waiting to be started again
after it ended:

```
 [ + ]  consoled
```

The command line is Debian's.

*check*, *new* and *edit* work on the services' files
(*service.toml*(5)), with the same reader Spark uses:

*check* [_name_ | _file_]...
	Check the file of each service named, or each file given by a path
	(one with a */* or ending in _.toml_), or every file in
	_/system/settings/spark/services_. Prints _file_*: ok* for each that
	reads, and for each that does not, which line is wrong and why on
	the standard error; says which files are of before Phase 13 and
	not converted yet.

*new* _name_
	Write a new file for the service _name_ with every setting
	explained, which starts nothing at boot (*enabled = false*) and runs
	_/system/commands/true_ until it is edited. Refuses a name that is
	taken.

*edit* _name_
	Edit the service's file as *visudo*(8) edits sudoers: copy it to
	_.name.toml.edit_ beside it, open the copy in *$VISUAL*, *$EDITOR*
	or *nano*(1), and once the editor ends, check it. A copy that reads
	takes the file's place; one that does not shows what is wrong and
	asks *What now? (e)dit again, or e(x)it without saving*. The file
	changes only when its new text reads. Spark reads it when the
	service next starts: *service* _name_ *restart*.

*secret* keeps the secrets a service's file names, such as a database's
password, which Spark hands it as read-only files (*service.toml*(5),
ADR 0217). No command shows a value back.

*secret set* _name_ _secret_
	Set the service's secret: read its value from the terminal without
	showing it, or else from the standard input, less one line end
	after it, and keep it in
	_/system/state/spark/secrets/name/secret_, mode 0600. An empty
	value is refused. The service reads it when it next starts.

*secret list* _name_
	List the names of the service's secrets that are set.

*secret remove* _name_ _secret_
	Remove one.

# EXIT STATUS

0 for a request Spark took, and for *status* of a running service; 3
for *status* of a service that does not run; 4 for a service Spark does
not know; 1 when Spark refused the request or could not be reached; 2
for a command it does not know. *check* gives 1 when a file does not
read, *new* when the file is there already, *edit* when the edit
was given up, and *secret* when the secret could not be kept or removed.

# FILES

_/system/settings/spark/services/_*name*_.toml_
	The services' files (*service.toml*(5)).

_/system/live/spark/control_
	Spark's control socket.

_/system/state/spark/secrets/_*name*
	The service's secrets.

# SEE ALSO

*spark*(8), *service.toml*(5), *shutdown*(8), *systools*(1)
