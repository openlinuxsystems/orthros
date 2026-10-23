# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Patterns, with extglob: for each pattern and text below, [[ == ]], case,
# and ${s#p} ${s##p} ${s%p} ${s%%p} ${s/p/R} ${s//p/R}, empty matches
# included; then patterns on long text, which must not take long.
shopt -s extglob
patterns=(
	'a*' '*a' '*' '?' '??' '?b' 'a?c' '[ab]*' '[!a]?' '[a-c]b' '[[:alpha:]]*' '*[[:digit:]]'
	'\*' 'a\?' '*.c' '.*' '[]a]' '[!]]*' '@(a|b)c' '?(a)b' '*(ab|c)' '+(a|bc)' 'a*(b)'
	'*.@(c|h)' '+([[:digit:]])' '@(ab|a)b' '*(a)?' '?(ab)*(c)' 'x+(y|z)' '@(a*|*b)'
)
texts=(a b ab ba abc aab bcbc abab cab x.c x.h .x '*' 'a?' 'a b' ']a' 123 xyzy xa ccc)
for p in "${patterns[@]}"; do
	for s in "${texts[@]}"; do
		[[ $s == $p ]] && r=1 || r=0
		case $s in $p) c=1 ;; *) c=0 ;; esac
		printf '%s|%s|%s|%s|%s|%s|%s|%s|%s|%s\n' "$p" "$s" $r $c \
			"${s#$p}" "${s##$p}" "${s%$p}" "${s%%$p}" "${s/$p/R}" "${s//$p/R}"
	done
done
# An empty pattern replaces nothing; anchored, it adds at either end.
s=abc
echo "${s//''/R}" "${s/''/R}" "${s/#/R}" "${s/%/R}" "${s//?(x)/-}" "${s/#?(a)/-}"
# Long text: each must be quick, as each look at it is. (bash takes
# minutes for +(ab) on the 40 KB text, so it has less.)
long=$(printf 'ab %.0s' {1..10000})
short=${long:0:600}
start=$SECONDS
a=${long//[[:space:]]/}
b=${short//+(ab)/x}
c=${long%%b*}
d=${long##*a}
[[ $short == *(ab )ab?(x) ]] && e=yes || e=no
echo ${#a} ${#b} ${#c} ${#d} $e $(( SECONDS - start < 5 ))
