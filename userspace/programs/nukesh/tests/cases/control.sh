# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Compound commands and functions.
if false; then echo 1; elif true; then echo 2; else echo 3; fi
i=0; while [ $i -lt 3 ]; do echo $i; i=$((i+1)); done; until [ $i -eq 0 ]; do i=$((i-1)); done; echo $i
for x in a b c; do case $x in a) echo first ;; b) continue ;; *) echo "other $x" ;; esac; done
for i in 1 2 3; do for j in 1 2 3; do [ $j = 2 ] && continue 2; [ $i = 3 ] && break 2; echo $i$j; done; done
case hello in h*o) echo glob ;; *) echo no ;; esac
case x in [a-z]) echo class ;; esac; case '*' in \*) echo star ;; esac
case foo in f*|b*) echo fb ;; esac; case a in b) ;; esac; echo "empty case $?"
case 1 in 1) echo one ;& 2) echo fell ;; 3) echo no ;; esac
case ab in a*) echo a-star ;;& *b) echo star-b ;;& c) echo c ;; esac
x=foo; case $x in "$x") echo quoted-pattern ;; esac
f() { echo "args: $#"; return 3; }; f a b; echo "rc=$?"
g() { local v=inner; h; }; h() { echo "v=$v"; }; v=outer; g; echo "v=$v"
outer() { inner() { echo "inner $1"; }; inner "$@"; }; outer x y
function kw { echo "keyword $1"; }; kw a
fact() { if (( $1 <= 1 )); then echo 1; else echo $(( $1 * $(fact $(( $1 - 1 ))) )); fi; }; fact 10
counter() { local n=$1; (( n > 0 )) || return 0; counter $((n - 1)); echo $n; }; counter 3
r() { return 300; }; r; echo "return mod $?"
f() { return; }; false; f; echo "f=$?"
{ echo a; echo b; } | wc -l
(exit 42); echo $?
true && { echo grp; }; false || echo or; ! false && echo not
x=1; { x=2; }; echo $x; (x=3); echo $x
[[ "abc" =~ ^a(b)c$ ]] && echo "${BASH_REMATCH[0]} ${BASH_REMATCH[1]}"
re='^([0-9]+)\.([0-9]+)$'; [[ 12.34 =~ $re ]] && echo "${BASH_REMATCH[2]}.${BASH_REMATCH[1]}"
[[ sign_csr =~ sign_domains|sign_csr ]] && echo "alternation"; [[ "|" =~ [|] ]] && echo "bar"
[[ y =~ x||y && 1 -eq 1 ]] && echo "empty alternative"
x=1; [[ $x -eq 1 && ( 2 -gt 1 || 0 -eq 1 ) ]] && echo complex
[[ a < b ]] && echo lt; [[ 10 -lt 9 ]] || echo numlt; [[ -z "" && -n x ]] && echo zn
[[ foo == f* ]] && echo pattern; [[ foo == "f*" ]] || echo literal
[[ -e /tmp && -d /tmp && ! -f /tmp ]] && echo files
shopt -s nocasematch; [[ ABC == abc ]] && echo nocase; shopt -u nocasematch
time_out=$( { time true; } 2>&1 ); [ -n "$time_out" ] && echo timed
