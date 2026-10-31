notifyd(8) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

notifyd - pass notifications to the screen

# SYNOPSIS

*notifyd*

# DESCRIPTION

notifyd takes notifications from programs and passes them to the
programs that show them: *consoled*(8) shows each on the bottom line of
the screen (ADR 0212). Spark runs it as the *notifyd* service, before
consoled.

A program sends a notification with *notify-send*(1), or by writing a
request, a line of JSON, to notifyd's socket,
_/system/live/notify/socket_:

```
{"ask":"send","title":"Backup done","body":"All 3 disks are copied.",
 "urgency":"normal","seconds":8,"app":"backup"}
```

(one line). Only *title* is needed. *urgency* is *low*, *normal* (the
default) or *critical*; *seconds* how long to show it, 8 by default, at
most 60, and 0 for until a key is pressed, which a critical one always
is; *app* a name the program goes by. notifyd answers with the
notification's number, *{"id":7}*, or why it refused it,
*{"error":"..."}*, and closes the connection.

notifyd puts on each notification the number of the process that sent
it and that process's name, as the kernel gives them, whatever the
request says: a name in *app* is shown after the program's own, as
*notify-send (backup)*, never alone. It cuts a title to 120 characters
and a body to 1000, shows a tab as a space and any other control
character as *?*, and keeps no newline in a title.

A process may send 10 notifications in any 10 seconds; notifyd refuses
more, and logs once that it does.

notifyd keeps the last 64 notifications in memory and nowhere else:
nothing of a notification goes to the disk or to the log, and they are
gone when notifyd stops. *notifications*(1) lists them. The request

```
{"ask":"list","count":5,"follow":true}
```

answers with the newest 5 kept (all without *count*), a line of JSON
each, and with *follow* goes on sending each new one as it comes:

```
{"id":7,"time":"2026-10-30T23:40:01.123456Z","process":57,
 "name":"notify-send","app":"backup","urgency":"normal",
 "title":"Backup done","body":"All 3 disks are copied.","seconds":8}
```

A connection has 5 seconds to send its request, and a follower 5
seconds to read what it is sent, or notifyd closes it.

Until Dukovo has users every program can send and list notifications;
once it has them, each user's go to their own consoles.

# FILES

_/system/live/notify/socket_
	The socket notifyd answers on.

_/system/settings/spark/services/notifyd.toml_
	The service.

# EXIT STATUS

As a service, notifyd does not exit but when it is stopped, with 0; it
exits with 1 if it cannot start.

# SEE ALSO

*notify-send*(1), *notifications*(1), *consoled*(8), *spark*(8),
*dukovo-privacy*(7)
