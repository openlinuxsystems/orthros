service(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

service - start, stop and look at the services

# SYNOPSIS

*service* _name_ *start* | *stop* | *restart* | *--full-restart* | *status*

*service --status-all*

# DESCRIPTION

service asks Spark, over its control socket _/run/spark/control_
(*spark*(8)), about the services it supervises: the programs that
_/system/etc/spark/services_ describes, a file each.

*start* starts a service that is stopped or disabled, *stop* stops one
and keeps it stopped until it is started again or the system restarts,
and *restart* stops it and starts it again; Spark stops the domain of
one that has not ended 5 seconds after SIGTERM. *status* shows what
Spark knows of it:

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

# EXIT STATUS

0 for a request Spark took, and for *status* of a running service; 3
for *status* of a service that does not run; 4 for a service Spark does
not know; 1 when Spark refused the request or could not be reached; 2
for a command it does not know.

# FILES

_/system/etc/spark/services/_*name*_.service_
	The services.

_/run/spark/control_
	Spark's control socket.

# SEE ALSO

*spark*(8), *shutdown*(8), *systools*(1)
