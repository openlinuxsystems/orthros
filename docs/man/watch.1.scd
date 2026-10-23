watch(1) "Dukovo" "Dukovo Manual"

; SPDX-FileCopyrightText: 2026 The Dukovo contributors
; SPDX-License-Identifier: Apache-2.0 OR MIT

# NAME

watch - run a command again and again, showing its output

# SYNOPSIS

*watch* [*-bdegptx*] [*-n* _seconds_] _command_ [_argument_ ...]

# DESCRIPTION

watch runs _command_ every two seconds, as procps-ng's watch does, and
shows its output, and what it writes to standard error, full screen,
until interrupted. A title line at the top says how often, what, where
and when. Output longer or wider than the screen is cut to it.

The words of _command_ are joined with spaces and run with *sh -c*, so
that pipes and other shell syntax work when quoted; with *-x* they are
run as they are.

# OPTIONS

*-n*, *--interval* _seconds_
	Run it every _seconds_, which may have a fraction (with a point or a
	comma), not less than 0.1.

*-p*, *--precise*
	Run it at exact multiples of the interval from the first time,
	however long it takes, not the interval after each run ends.

*-d*, *--differences*[*=permanent*]
	Show what changed since the run before in reverse video; with
	*permanent*, everything that has changed since the first.

*-g*, *--chgexit*
	Exit when the output changes.

*-e*, *--errexit*
	When the command fails, stop, say so and wait for a key, then exit
	with status 8.

*-b*, *--beep*
	Beep when the command fails.

*-t*, *--no-title*
	No title line.

*-x*, *--exec*
	Run the command's words as they are, not with *sh -c*.

*-c*, *-C*, *-w*, *--color*, *--no-color* and *--no-wrap* are taken and
change nothing: the output is shown without its colours, and cut.

# EXIT STATUS

0 when interrupted, or with *-g* when the output changed; 8 with *-e*
when the command failed; 1 if the command could not be run.

# EXAMPLES

Watch memory while a build runs:

	watch -n 1 free -h

Wait for a file to appear:

	watch -g ls /volumes

# SEE ALSO

*top*(1), *nukesh*(1), *systools*(1)
