# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# What scripts for sh rely on, as dash does it: found by the shell corpus.
echo joined\
line
alias say='echo said'
say "an alias in a script"
x=5 y=$((x + 2)) printenv y
VAR=temporary command printenv VAR
i=0; x=$((i += 1)) printenv x > /tmp/posix-scripts-$$-$((i += 10)); cat /tmp/posix-scripts-$$-*
rm -f /tmp/posix-scripts-$$-*
set -- a b; unset v; : "${v=$*}"; echo "$v"
sleep 0 & [ "$!" -gt 0 ] && echo "a background job's ID"; wait
trap 'echo x' KILL; echo "trap KILL: $?"
echo lost >&-; echo "echo to a closed descriptor: $?"
for i in 1 2; do (break); echo "i=$i"; done
trap 'echo bye' EXIT
set -m; echo "set -m: $?"; set +m
command exec 3>/nonesuch-posix-scripts/x || echo "command exec failed, and the script goes on"
readonly ro=1; command readonly ro=2; echo "command readonly: $?"
dir=/tmp/posix-scripts-$$; mkdir "$dir" && cd "$dir" && echo .*; cd / && rm -r "$dir"
set -- ${unset_var:-a b}; echo "default split: $#"
on=1; set -- ${on:+-f }file; echo "alternative split: $# $1"
IFS=:; set -- ${unset_var:-a:b}; echo "split by IFS: $#"; IFS=' '
sleep 0 & wait; echo "[${!##*}]"
(set -e; if true; then false && true; fi; { false && true; }; echo "set -e goes on")
(set -e; f() { false && true; }; f; echo "not reached"); echo "a function's failure ends it: $?"
f() { set -Q; echo "not reached"; }
f
echo "not reached"
