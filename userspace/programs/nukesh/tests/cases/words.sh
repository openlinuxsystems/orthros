# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Quoting, word splitting, brace expansion and pathname expansion.
echo "a b" 'c d' e\ f
printf '[%s]' "$@" x; echo
echo "a\"b" 'a"b' a\"b 'it'"'"'s'
echo $'tab\there' $'\x41\101' $'é'
v="a  b   c"; echo $v; echo "$v"
set -- 1 "2 3" 4; echo $#; for a; do echo "[$a]"; done; echo "$*"
(IFS=:; echo "$*")
(IFS=,; x='a,b,,c'; set -- $x; echo $#; for i; do printf '[%s]' "$i"; done; echo)
(IFS=' :'; x=' a : b  c:'; set -- $x; echo $#; printf '<%s>' "$@"; echo)
set -- 'a b' c; for x in "$@"; do echo "<$x>"; done; for x in $@; do echo "($x)"; done
echo "${@}" "${*}" "${#@}"
echo {a,b,c}{1,2} {1..5} {a..e..2} {05..10..2} {3..1} x{,y}z {a,b{c,d}}
echo \{a,b} "{a,b}" {a} {a..}
echo $(printf 'a\n\n\n'); echo "$(printf 'a\n\n')"x
newline='
'; echo "a${newline}b"
echo "nested $(echo "inner $(echo deep)")" `echo back\`echo tick\``
dir=/tmp/nukesh-words.$$
mkdir -p "$dir" && cd "$dir" || exit 1
touch b.txt a.txt c.log .hidden 'with space'
echo *.txt; echo *; echo .*; echo ?.log; echo [ab].*; echo [!a].txt
echo *.none; (shopt -s nullglob; echo *.none end); (shopt -s failglob; echo *.none) 2>/dev/null || echo failglob
(shopt -s dotglob; echo *)
shopt -s extglob
echo !(*.txt); echo @(a|c).*; echo +([a-c]).txt
shopt -u extglob
(set -f; echo *.txt)
for f in *; do echo "file: $f"; done
x='*.txt'; echo $x "$x"
# globstar: a link to a directory is listed by **/ but not gone into;
# dir/** is dir/ too; what comes before a pattern is kept as written.
mkdir -p d/e real/inner && touch d/f real/g real/inner/h
ln -s real link && ln -s d/e elink && ln -s nowhere dangling
shopt -s globstar
echo **/; echo ./**/; echo **; echo real/**; echo r*/**; echo **/inner/**
echo */**; echo real//*; echo r*//g; echo **/g; echo link/**; echo d/f/**
shopt -u globstar
cd / && rm -r "$dir"
case ~ in /*) echo tilde ;; esac; echo "~" '~'; HOME=/home/x; echo ~/a ~nobody_here/b
a=~; echo $a; b=x:~/y; echo $b
