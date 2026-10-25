# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Coprocesses: a command in the background with pipes to its standard
# input and output, its ends in an array, as bash runs it.
coproc cat
[ "$!" = "$COPROC_PID" ] && echo "its pid is \$!"
(( COPROC[0] > COPROC[1] && COPROC[1] > 9 )) && echo "high descriptors"
echo hello >&"${COPROC[1]}"; read -r line <&"${COPROC[0]}"; echo "got $line"
# A subshell, a pipeline's commands and `&` close them; command and
# process substitutions keep them.
(echo x >&"${COPROC[1]}") 2>/dev/null || echo "closed in a subshell"
{ echo x >&"${COPROC[1]}"; } 2>/dev/null | cat; echo "closed in a pipeline ${PIPESTATUS[0]}"
{ echo x >&"${COPROC[1]}"; } 2>/dev/null & wait $!; echo "closed in the background $?"
x=$(echo substituted >&"${COPROC[1]}"); read -r line <&"${COPROC[0]}"; echo "got $line"
cat <(echo processed >&"${COPROC[1]}"); read -r line <&"${COPROC[0]}"; echo "got $line"
jobs
# A second one while the first runs is warned of.
msg=$({ coproc SECOND { :; }; } 2>&1); echo "$msg" | sed 's/^.*warning/warning/; s/\[[0-9]*:/[PID:/'
# Closing its input ends cat; once the shell has seen it end, its
# descriptors close and its variables go.
pid=$COPROC_PID
eval "exec ${COPROC[1]}>&-"
declare -p COPROC | sed 's/"[0-9][0-9]*"/"N"/'
wait "$pid"; echo "cat ended $?"
echo "${COPROC[@]-no array} ${COPROC_PID-no pid}"
# A compound command can be named.
coproc LOOP { while read -r l; do echo "[$l]"; done; }
echo one >&"${LOOP[1]}"; read -r line <&"${LOOP[0]}"; echo "$line"
jobs
kill "$LOOP_PID"; wait "$LOOP_PID"; echo "killed $?"
coproc EXIT3 (exit 3)
wait "$EXIT3_PID"; echo "subshell $?"
coproc NAMED while false; do :; done
wait; echo "${NAMED[@]-gone}"
# A word after `coproc` is a name only before a compound command.
coproc echo one two; read -r line <&"${COPROC[0]}"; echo "$line"; wait
coproc x=assigned printenv x; read -r line <&"${COPROC[0]}"; echo "$line"; wait
# How functions show them. (bash shows the first as `coproc COPROC cat
# -u`, which reads back as a command named COPROC.)
f1() { coproc cat -u; }
f2() { coproc NAME { cat; } >/dev/null; }
f3() { coproc W while read -r l; do echo "$l"; done; }
f4() { coproc N2 ( echo a ) & }
declare -f f1 f2 f3 f4
# Mistakes.
msg=$({ coproc 1x { cat; }; } 2>&1); echo "status $? ${msg#*: }"
for text in 'coproc' 'coproc ! true' 'coproc N then' 'coproc N function g { :; }' 'true | !'; do
	(eval "$text") 2>/dev/null; echo "$text: $?"
done
# `time` and `!` alone time and negate nothing.
!; echo "bang $?"
time -p 2>/dev/null; echo "time $?"
! time -p true 2>/dev/null; echo "both $?"
eval 'coproc coproc <&-' 2>/dev/null; echo "coproc coproc $?"
