# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Background jobs and waiting for them.
sleep 0.1 & wait $!; echo "waited $?"
(exit 7) & wait $!; echo "status $?"
for i in 1 2 3; do (echo "job $i") & wait; done; echo "all jobs done"
(sleep 0.2; exit 3) & a=$!; (exit 4) & b=$!; wait $a; echo "a $?"; wait $b; echo "b $?"
(sleep 1; exit 2) & (exit 1) & wait -n; echo "first $?"; wait
sleep 5 & pid=$!; kill $pid; wait $pid; echo "killed $?"
sleep 5 & kill %1; wait %1; echo "by job $?"
wait; echo "nothing to wait for $?"
wait 99999 2>/dev/null; echo "not a child $?"
echo "$!" | while read -r p; do [ -n "$p" ] && echo has-last-pid; done
{ echo grouped; } & wait
x=outer; { x=inner; } & wait; echo $x
# A command a signal ended is reported on standard error, as bash does:
# its PID, how it ended and its command, for each of a pipeline's; SIGTERM
# by name alone, SIGINT not at all; in a function, at its body's first line.
# PIDs, paths and core dumps (cores piped to a program ignore -c 0) are cut.
{
	ulimit -c 0
	sh -c 'kill -SEGV $$'
	true | sh -c 'exit 3' | sh -c 'kill -BUS $$'
	sh -c 'kill -TERM $$'
	sh -c 'kill -INT $$'
	( : ; sh -c 'kill -ILL $$' )
	f() {
		sh -c 'kill -SEGV $$'
	}
	f
	sh -c 'kill -ABRT $$' &
	wait
	x=$(sh -c 'kill -SEGV $$')
} 2>&1 | sed -E 's/^[^ ]* line ([0-9]+): +[0-9]+ /line \1: PID /; s/^ +[0-9]+ /     PID /; s/\(core dumped\) //'
# The parent's jobs are listed in a command substitution and in a
# pipeline's commands, as bash lists them, but not in a subshell, so a
# throttle works.
sleep 5 & echo "substituted $(jobs -p | wc -l)"; jobs | sed 's/  */ /g'; (jobs | wc -l); kill %1; wait
for i in 1 2 3 4 5; do
	while [ "$(jobs -rp | wc -l)" -ge 2 ]; do wait -n; done
	(sleep 0.1) &
	[ "$(jobs -rp | wc -l)" -le 2 ] || echo "too many"
done; wait; echo "throttled"
# Loops, `if` and `case` are laid out on lines of their own in a job's
# text, as bash lays them out.
while :; do sleep 5; done & if :; then sleep 5; fi & case x in x) sleep 5;; esac & jobs; kill %1 %2 %3; wait
# With job control a background job keeps its input.
echo kept | { set -m; cat & wait; }
# A script's jobs that ended are kept until `jobs` tells of them, as bash
# keeps them; `wait` takes one away untold.
true & sleep 0.5; jobs; (exit 3) & sleep 0.5; jobs; jobs; echo "listed"
sh -c 'exit 4' & sleep 0.5; wait $!; echo "waited $?"; jobs; echo "none left"
false & sleep 0.5; true & sleep 0.5; jobs %1; jobs
# A command substitution runs without job control, as bash's does: a
# pipeline in it stays in the shell's process group, so a program in it
# that sets the terminal is not stopped as a background job's would be.
set -m
mine=$(ps -o pgid= -p $$)
piped=$(sh -c 'ps -o pgid= -p $$' | cat)
[ "$piped" = "$mine" ] && echo "a command substitution's pipeline: the shell's group"
set +m
