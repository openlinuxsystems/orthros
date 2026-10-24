# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Functions as declare -f and type show them: as bash lays them out.
f1() { echo a > out 2>&1 < in >> app 2>/dev/null; cat <<EOF
line $x
EOF
cat <<'Q'
raw $y
Q
cat <<-T
	tab
	T
echo after; }
f2() { a | b |& c; ! a | b; time a; time -p b; a && b || c; a & b; }
f3() { case $x in a|b) p;; c) q;& d) r;;& *) s;; esac; case y in esac; }
f4() { (( x = 1 + 2 )); for ((;;)); do break; done; let x++; [[ a == b && c != d || -f e ]]; [[ a =~ ^x(y)$ ]]; [[ a < b ]]; }
f5() { local a=1 b; declare -a arr=(1 2 "3 4"); arr+=(5); x=${y:-z} z=$(( 1+2 )) cmd arg; export E=1; }
function f6 { echo kw; }
f7() { f8() { echo inner; }; f8; }
f9() { echo "$(cmd "q")" $'ansi\n' "${x[@]}" ${#y} ${z//a/b} 'it'\''s'; }
f10() { while read -r l; do echo "$l"; done < file; { a; b; } | c; if ! a; then :; fi; }
f11() { echo a; # comment
echo b; }
f12() { exec 3>&1 4<&- 5<>f >&- <&6; echo {fd}>f; a <<< "here"; }
f13() { select x in "a b" c; do echo $x; break; done; until false; do :; done; }
f14() { [[ ! -z $a ]] && echo; [[ ( a ) && ( b || ! ( c ) ) ]]; [[ x ]]; }
f15() { a 2>&1 | b; a > >(b) 2> >(c); d < <(e); }
f16() { echo "a
b"; x='single
quote'; }
f17() { if a; then b; c; elif d; then e; elif g; then h; else i; fi; }
f18() { ( a ); ( a; b; c ); ( a; b ) | c; ( if x; then y; fi ); ( a & ); ( a & b ); { a & b; }; }
f19() { g() ( a; b ); function k { y; } > out; if a; then m() { z; }; fi; }
f20() ( subshell body )
for f in f1 f2 f3 f4 f5 f6 f7 f9 f10 f11 f12 f13 f14 f15 f16 f17 f18 f19 f20; do declare -f $f; done
f7; declare -f f8
type f6
# A parameter written in braces is shown in braces, as written.
f21() { echo ${x}y "${x}" $x ${1} ${@}; }; declare -f f21
