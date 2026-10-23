# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The POSIX shell, as sh: matched against dash.
echo "a b" 'c d' e\ f
set -- 1 "2 3" 4; echo $#; for a; do echo "[$a]"; done; echo "$*"; (IFS=:; echo "$*")
x=hello; echo ${x#h} ${x%lo} ${#x} ${x:-def} ${y:-def} ${y:=set}; echo $y
echo $(( 2*10 )) $(( 7/2 )) $(( 7%3 )) $(( 1<<4 )) $(( (3>2) && (1||0) )) $(( 0x1f )) $(( 017 )) $(( -5 / 2 ))
i=0; while [ $i -lt 3 ]; do echo $i; i=$((i+1)); done
f() { echo "args: $#"; return 3; }; f a b; echo "rc=$?"
f() { v=inner; }; v=outer; f; echo "v=$v"
case "hello" in h*o) echo glob ;; *) echo no ;; esac
case x in [a-z]) echo class ;; esac; case '*' in \*) echo star ;; esac
false | true; echo $?; ! true; echo $?
x=$( { echo out; echo err >&2; } 2>&1 ); echo "$x"
{ echo a; echo b; } > /tmp/nukesh-p1.$$; cat /tmp/nukesh-p1.$$; echo c >> /tmp/nukesh-p1.$$; wc -l < /tmp/nukesh-p1.$$; rm /tmp/nukesh-p1.$$
printf '%5s|%-5s|%05d|%x|%o|%c|%%\n' ab cd 42 255 8 xyz
printf '%s\n' a b c | while read line; do echo "got $line"; done
(trap 'echo trapped' EXIT; echo before)
(set -e; false || echo ok; (false); echo notreached); echo "status $?"
(set -u; echo ${undefined_var-ok}; echo $undefined_var2) 2>/dev/null; [ $? -ne 0 ] && echo "nounset failed"
x=5; y=$((x * 2)); echo $x $y
cd /tmp && pwd; cd - >/dev/null; echo back
command -v cd; type cd >/dev/null; echo typed
alias ll='ls -l'; alias ll; unalias ll
(readonly r=1; r=2; echo "after") 2>/dev/null; [ $? -ne 0 ] && echo "readonly failed"
sleep 0.1 & wait $!; echo "waited $?"
(exit 42); echo $?
echo "nested $(echo "inner $(echo deep)")"
v="a  b   c"; echo $v; echo "$v"
set -- a b c; shift; echo "$@"; shift 2; echo $#
echo -n "x"; echo; echo 'a\tb'
if false; then echo 1; elif true; then echo 2; else echo 3; fi
for x in /nonexistent*; do echo "$x"; done
unset x; echo ${x-unset} ${x+set}; x=; echo ${x-unset} ${x+set} ${x:-empty}
eval 'echo eval $((2+2))'; e='echo from var'; eval "$e"
umask 022; umask
gt() { OPTIND=1; while getopts "ab:c" opt; do echo "$opt ${OPTARG:-}"; done; shift $((OPTIND-1)); echo "rest: $*"; }; gt -a -b val -c file
x='a b'; set -- $x; echo $#; set -f; x='*'; echo $x; set +f
set -- $(echo 1 2 3); echo $#
(IFS=,; x='a,b,,c'; set -- $x; echo $#; for i; do printf '[%s]' "$i"; done; echo)
read a b <<EOF
one two three
EOF
echo "$a|$b"
cat <<'EOF'
literal $x `y`
EOF
x=1; { x=2; }; echo $x; (x=3); echo $x
echo $((1+2)) $((10 > 9)) $((5 == 5 ? 7 : 8))
test -z ""; echo $?; test "a" != "b"; echo $?; [ 1 -eq 1 -a 2 -eq 2 ]; echo $?
set -- 'a b' c; for x in "$@"; do echo "<$x>"; done; for x in $@; do echo "($x)"; done
echo $(printf 'a\n\n\n'); echo "$(printf 'a\n\n')"x
kill -0 $$; echo alive
trap 'echo int' INT; kill -INT $$; echo after-int; trap - INT
cmd=echo; $cmd hi; "$cmd" there
printf '%d %s\n' 1 a 2 b 3
echo "${x:+set}" "${unset_var:+no}"
a=1; b=2; [ "$a" -lt "$b" ] && echo less
times >/dev/null; echo times
(shift 5) 2>/dev/null || echo "shift failed"
f() { return; }; false; f; echo "f=$?"
echo "a\"b" 'a"b' a\"b
x=abc; echo "${x}def" "${x%c}" "${x#*b}" "${x%%*}" "${x##*}"
pwd -P >/dev/null; echo pwd
printf "%s" "no newline"; echo
newline='
'; echo "a${newline}b"
