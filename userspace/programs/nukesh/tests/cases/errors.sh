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
# A syntax error in eval's text or in a file `.` reads, and a file `.`
# cannot read, fail the builtin, and the script goes on.
eval "if"; echo "eval syntax: $?"
h() { eval "fi"; echo "in h: $?"; }; h
echo "if" > "$dir/syntax"; . "$dir/syntax"; echo "source syntax: $?"
. "$dir/missing"; echo "source missing: $?"
. "$dir"; echo "source directory: $?"
# A program whose `#!` interpreter is missing.
printf '#!/no/such/interpreter\n' > "$dir/badi"; chmod +x "$dir/badi"
msg=$(badi 2>&1); echo "missing interpreter: $? ${msg##*: }"
rm -r "$dir"
# `exit` and `return` with more than one operand fail their command.
exit 3 4; echo "not reached"
echo "after exit: $?"
f() { return 3 4; echo "not reached"; }; f; echo "not reached"
echo "after return: $?"
# mapfile's callback comes after every quantum lines, before the last is
# stored, with it in single quotes; a syntax error in it goes no further.
mapfile -C 'echo cb' -c 2 lines < <(printf '%s\n' 1 2 3 "it's"); echo "mapfile: $? ${#lines[@]}"
mapfile -s 1 -O 5 -C 'printf "<%s %s>\n"' -c 1 -t lines < <(printf '%s\n' a b c)
msg=$( { mapfile -C '[[ a b ]]' -c 1 x <<< 1; echo "goes on: $?"; } 2>&1 )
while IFS= read -r line; do echo "${line#*: stdin: }"; done <<< "$msg"
# Status operands are decimal numbers of 64 bits; return outside a
# function and shift with no number fail.
for v in 010 0x10 " 7 " +7 -1 9223372036854775807 9223372036854775808 "" 7x; do
  (exit "$v"); echo "exit [$v]: $?"
done
return; echo "return outside: $?"
shift x; echo "shift x: $?"
# A directory read by `$(<` is nothing, as bash reads it.
x=$(< /); echo "directory: $? [$x]"
# The pattern of an unset parameter is not expanded.
unset u; echo "<${u/$((1/0))/}> <${u#$((1/0))}> <${u%%$((1/0))}> <${u^$((1/0))}>"
v=; echo "<${v/$((1/0))/}>"; echo "not reached"
echo "after set pattern: $?"
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
