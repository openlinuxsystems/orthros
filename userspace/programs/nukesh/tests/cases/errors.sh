# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Errors that abandon a command: the script goes on with its next one, as
# bash's does. Arithmetic in a command only fails it.
echo $((1+)); echo "not reached"
echo "after arithmetic: $?"
x=; if (( $x > 0 )); then echo positive; else echo "not positive: $?"; fi
let 1+; echo "let: $?"
[[ 1 -eq 1+ || 2 -eq 2 ]]; echo "[[: $?"
for ((i = 0; i < 1+; i++)); do echo loop; done; echo "for: $?"
a=(1 2); echo "${a[1+]}"; echo "not reached"
echo "after index: $?"
readonly r=1
r=2; echo "not reached"
echo "after read-only: $?"
for r in a b; do echo "loop $r"; done; echo "for read-only: $?"
f() { echo "in f"; echo ${!#3}; echo "not reached"; }
f; echo "not reached"
echo "after f: $?"
eval 'echo $((2+)); echo "not reached"
echo "eval goes on"'; echo "eval: $?"
(echo $((3+)); echo "not reached"); echo "subshell: $?"
v=$(echo $((4+)); echo "not reached"); echo "substitution: [$v] $?"
while read -r n; do echo "read $n"; echo $((n+)); done <<< $'1\n2'
echo "after while: $?"
: ${1:=one}; echo "not reached"
echo "after cannot assign: $?"
# A bad substitution where it never runs.
if false; then y=${x@Z}; fi; echo "untaken"
s=:a:b:; echo "${s:1:${#s}-2}" "${s: -2}" "${s:${#s}-3:2}" "${s:$((1)):1}" "${s:1?2:3}"
unset u; echo "[${u:1+}]"
echo "${BASH_VERSINFO[0]}" "${#BASH_VERSINFO[@]}"; BASH_VERSINFO=1; echo "not reached"
# A file on PATH that is not executable is found, and fails to run with
# 126. On Radix, whose files have mode 0755 until users come
# (radix-posix(7)), it runs, as a script.
dir=/tmp/nukesh-errors-$$; mkdir "$dir"; echo 'echo ran' > "$dir/plain"
PATH=$dir:$PATH
type -P plain >/dev/null && echo "plain is found"
if [ -x "$dir/plain" ]; then want=0; else want=126; fi
plain >/dev/null 2>&1; got=$?
[ "$got" = "$want" ] && echo "plain: as its mode says" || echo "plain: $got"
rm -r "$dir"
# Indirect expansions: an unset name is an error, an unset element or
# parameter is not, and `:=` assigns to the variable named.
echo "<${!no_such_name:-d}>"; echo "not reached"
echo "after unset name: $?"
a=(x); declare -A m; g() { echo "<${!1}> <${!a[1]}> <${!m[k]}>"; }; g
n=target; : "${!n:=assigned}"; echo "<$target> <$n>"
n="a b"; echo "<${!n}>"; echo "not reached"
echo "after bad name: $?"
readonly empty=; : "${empty:=again}"; echo "not reached"
echo "after read-only default: $?"
set -u; echo "${!no_such_name}"; echo "not reached"
echo "indirect before set -u: $?"
echo "$unset_variable"; echo "not reached"
