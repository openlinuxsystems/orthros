# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Parameter expansion.
x=hello
echo ${x^^} ${x^} ${x:1:3} ${x: -2} ${x:1} ${x::2} ${x#h} ${x#*l} ${x##*l} ${x%lo} ${x%l*} ${x%%l*}
echo ${x/l/L} ${x//l/L} ${x/#h/H} ${x/%o/O} ${x//[el]/} ${#x}
echo ${x:-def} ${y:-def} ${y:=set} $y ${z-unset} ${x:+alt} ${z:+alt}
unset x; echo ${x-unset} ${x+set}; x=; echo ${x-unset} ${x+set} ${x:-empty} ${x:+alt}
(: ${q:?is required}) 2>/dev/null || echo "q failed $?"
str="hello world"; echo ${str^} ${str~~} ${str~} ${str,,} ${str^^[lo]}
up=HELLO; echo ${up,} ${up,,}
x='a*b'; echo "${x//\*/-}" ${x/#a/A} ${x/%b/B}
p=/usr/local/lib/libfoo.so.1; echo ${p##*/} ${p%/*} ${p%%.*} ${p#*.}
f=archive.tar.gz; echo "${f%.*}" "${f%%.*}" "${f#*.}" "${f##*.}"
v=abc; echo ${v:0:1}${v: -1} ${v:5} "[${v:1:0}]"
name=v; echo ${!name}; pre_a=1 pre_b=2; echo ${!pre_*}
set -- p q r; echo ${!#} ${@: -1} ${@:2:1} ${@:2} ${#}
x=1; a=(p 'q r'); echo ${x@Q} ${a[@]@Q}; s='it'"'"'s'; echo ${s@Q}
e='a\tb'; echo ${e@E}; u=word; echo ${u@U} ${u@u} ${u@L} ${u@A}
declare -i n=5; echo ${n@a}; declare -r ro=1; echo ${ro@a}
echo "${a:-$(echo dflt)}"
x=10; echo "${x:-a b}" | wc -w; unset x; echo "${x:-a b}" | wc -w
readonly r=1; (r=2) 2>/dev/null || echo "readonly kept"
[ -n "$-" ] && echo specials $? $#
echo ${PPID:+ppid} ${UID:+uid} ${PWD:+pwd} ${RANDOM:+random} ${SECONDS:+seconds}
echo $((RANDOM >= 0 && RANDOM < 32768)) ${#BASHPID} >/dev/null; echo randoms
declare -n ref=target; target=val; echo $ref; ref=new; echo $target
fn() { echo "${FUNCNAME[0]} ${#FUNCNAME[@]}"; }; fn
echo ${LINENO}
