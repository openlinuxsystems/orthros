# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Builtins.
echo -n "no newline"; echo; echo -e 'a\tb\x41\0101'; echo -E 'a\tb'; echo -
printf '%5s|%-5s|%05d|%x|%X|%o|%e|%.2f|%c|%%|%5.1f|%+d|% d\n' ab cd 42 255 255 8 1234.5 3.14159 xyz 2.25 3 4
printf '%s\n' a b c; printf '%d %s\n' 1 a 2 b 3; printf '%b\n' 'x\ty'; printf '%q\n' 'a b' "it's" $'x\ny'
printf -v out '%s-%s' a b; echo $out; printf '%s\n' "${@:-none}"; printf '%*d|%-*d|\n' 4 1 3 2
IFS=, read -r one two rest <<< "1,2,3,4"; echo "$one $two $rest"
x=$'line1\nline2'; while IFS= read -r l; do echo "<$l>"; done <<< "$x"
read -r -n 3 part <<< "abcdef"; echo $part; read -r -d : field <<< "a:b"; echo $field
read -r <<< "  spaced  "; echo "[$REPLY]"; IFS= read -r keep <<< "  spaced  "; echo "[$keep]"
read -r esc <<< 'a\b'; echo "$esc"; read noraw <<< 'a\b'; echo "$noraw"
gt() { local OPTIND opt; while getopts ":ab:c" opt; do case $opt in :) echo "missing $OPTARG";; \?) echo "bad $OPTARG";; *) echo "$opt ${OPTARG-}";; esac; done; shift $((OPTIND-1)); echo "rest: $*"; }
gt -a -b val -c file; gt -x; gt -b; gt -ac -- -d
cd /tmp && pwd; cd - >/dev/null; echo back; (cd /nonexistent 2>/dev/null) || echo "cd failed"
(cd /; cd tmp; echo $PWD; cd ..; echo $PWD; echo $OLDPWD)
type echo; type -t ls; type -t if; type -t type; command -v cd; f() { :; }; type -t f
alias ll='ls -l'; alias ll; unalias ll; alias ll 2>/dev/null || echo unaliased
declare -i n=5; n+=3; echo $n; declare -l lo=ABC; echo $lo; declare -u up=abc; echo $up
declare -x exported=1; env | sort | while IFS= read -r l; do [ "$l" = exported=1 ] && echo exported; done
umask 022; umask; umask -S; umask u=rwx,g=rx,o=; umask
kill -l 9 15; kill -l INT
eval 'echo eval $((2+2))'; e='echo from var'; eval "$e"; eval "a=1; b=2"; echo $a$b
test -z ""; echo $?; test "a" != "b"; echo $?; [ 1 -eq 1 -a 2 -eq 2 ]; echo $?; [ ! -e /nowhere ]; echo $?
[ -n "x" ] && [ "a" = a ] && [ 3 -gt 2 ] && test ! -z x && echo tests
[ abc \< abd ] && echo str-lt; [ \( 1 -eq 1 \) -o 1 -eq 2 ] && echo parens
set -- a b c; shift; echo "$@"; shift 2; echo $#; (shift 5) 2>/dev/null; echo "shift status $?"
set -- x y z; set -- "$@" w; echo $#; set --; echo $#
hash -r; hash ls 2>/dev/null; echo hashed
: ignored arguments; true; echo "colon $?"
times >/dev/null; echo times
v=1; unset v; echo "${v-gone}"; f() { :; }; unset -f f; type -t f || echo "f gone"
let 'q = 3 * 4'; echo $q
x=5 y=$((x + 1)); echo $x $y
s=$(printf '%s' "$(seq 1 3)"); echo "$s" | wc -l
shopt -q extglob && echo extglob-on || echo extglob-off; shopt -s extglob; shopt -q extglob && echo now-on
set -o | while read -r name state; do [ "$name" = errexit ] && echo "errexit $state"; done
command echo via-command; builtin echo via-builtin
caller_test() { caller 0 | { read -r line fn file; echo "called from $fn"; }; }; wrapper() { caller_test; }; wrapper
pushd /tmp >/dev/null; pushd / >/dev/null; dirs; popd >/dev/null; dirs; popd >/dev/null
