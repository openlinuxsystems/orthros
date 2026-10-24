# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Indexed and associative arrays.
a=(one two three); echo "${a[@]:1}" "${!a[@]}" ${#a[@]} ${a} ${a[1]} ${#a[1]}
a+=(four); echo ${a[-1]} ${a[@]: -2:1}; unset 'a[1]'; echo "${a[@]}" ${#a[@]} "${!a[@]}"
a[10]=ten; echo "${!a[@]}"; a=(); echo ${#a[@]}
declare -a sparse=([2]=two [5]=five); echo "${!sparse[@]}" "${sparse[@]}"
declare -A m; m[x]=1; m[y]=2; m["with space"]=3
for k in "${!m[@]}"; do echo "$k=${m[$k]}"; done | sort
echo ${#m[@]}; unset 'm[x]'; echo ${#m[@]}; [[ -v m[y] ]] && echo has-y; [[ -v m[x] ]] || echo no-x
declare -A c=([host]=localhost [port]=8080); echo "${c[host]}:${c[port]}"
mapfile -t lines < <(printf 'l1\nl2\nl3\n'); echo ${#lines[@]} ${lines[1]}
readarray -t -s 1 -n 1 picked < <(printf 'a\nb\nc\n'); echo "${picked[@]}"
IFS=';' read -ra pairs <<<"k1=v1;k2=v2"; for p in "${pairs[@]}"; do echo "${p%%=*} -> ${p#*=}"; done
arr=(c a b); IFS=$'\n' sorted=($(sort <<<"${arr[*]}")); unset IFS; echo ${sorted[*]}
x=(1 2 3); echo $(( x[0] + x[2] )) ${x[@]/2/two} ${x[*]^^}
words=("a b" c); copy=("${words[@]}"); echo ${#copy[@]}; joined="${words[*]}"; echo "$joined"
declare -p a sparse
arr2=(); arr2+=("a b"); arr2+=(c); echo "${#arr2[@]}: ${arr2[0]}"
n=(3 1 2); for i in "${!n[@]}"; do echo "$i:${n[i]}"; done
i=1; echo ${n[i+1]} ${n[$i]}
set -- x y; args=("$@"); echo ${#args[@]} "${args[1]}"
s=5; msg=$( { echo "x${s[-1]}x ${s[0]}"; } 2>&1 ); echo "${msg#*: line *: }"
q=(1 2); msg=$( { echo "y${q[-3]}y ${q[-1]}"; } 2>&1 ); echo "${msg#*: line *: }"
b=([-1]=4 5 [3]=6) 2>/dev/null; echo "${!b[@]} ${b[@]}"
q+=([-1]=9); echo "${q[@]}"
{ unset 'q[-9]'; echo "unset $?"; } 2>/dev/null
t=5; { unset 't[1]'; echo "scalar $?"; } 2>/dev/null; unset 't[0]'; echo "${t-gone}"
(unset 'q[1/0]'; echo not here) 2>/dev/null; echo "after $?"
a3=(1 2 3); (echo "n ${#a3[-4]}") 2>/dev/null; echo "length $? ${#a3[-1]} ${#nope[-1]}"
declare -l -a lw=(ABC); lw+=(DeF); lw=("${lw[@]}" GH); echo "${lw[@]}"
declare -i iv=(1+2 3*4); echo "${iv[@]}"
declare -u -A um=([k]=abc); declare -i -A im=([x]=1+2); echo "${um[k]} ${im[x]}"
# A subscript at a command's start runs to its `]`, blanks and operators
# too, as bash reads it; a key loses its quotes and backslashes, and
# associative arrays list their keys in bash's order.
a4[1 + 1]=x a4[ 3 ]+=y; x=1 a4[x | 4]=z; declare -p a4
declare -A m4; m4[a b]=1 m4[x;y]=2 m4[p&q]=3 m4[x|y]=4 m4[a<b]=5 m4[(p)]=6; declare -p m4
m4[a\]b]=7 m4['c]d']=8 m4["e f"]=9 m4[\$x]=10 m4[g\ h]=11 m4["i\]"]=12
echo "${m4[a\]b]} ${m4["c]d"]} ${m4[e f]} ${m4[\$x]} ${m4[g h]} ${m4[i\\\]]}"
m4=([p\]q]=1 [r s]=2 ["t"]=3); declare -p m4
for k in "${!m4[@]}"; do printf '%s=%s ' "$k" "${m4[$k]}"; done; echo
declare -A big; for ((i = 0; i < 3000; i++)); do big[k$i]=$i; done
unset 'big[k7]'; big[k7]=x; keys=("${!big[@]}")
echo "${#keys[@]} ${keys[0]} ${keys[1]} ${keys[1500]} ${keys[2999]}"
echo a5[1 b]=c
msg=$( { a5[1 b]; } 2>&1 ); echo "${msg#*: line *: }"
(a5[1 b]=c; echo "not reached") 2>/dev/null; echo "bad subscript: $?"
f4() { m4['a b']=1 m4["$k z"]=2 a4[i+1]=3; echo "${m4['a b']}" "${m4[a\]b]}"; }
declare -f f4
# A subscript in a name given as text is expanded, as bash expands it.
a6=(1 2 3 4); i=1; unset 'a6[$i]'; declare -A m6=([x]=1 [y]=2); k=x; unset 'm6[$k]'; echo "${!a6[@]} ${!m6[@]}"
printf -v 'a6[$i]' x; read 'a6[$((i+1))]' <<< r; [[ -v 'a6[$i]' ]] && test -v 'a6[$i]' && echo "${a6[*]}"
declare 'a6[$i]=9'; n='a6[$i]'; echo "${a6[1]} ${!n}"; export 'e6[1]=1' 2>/dev/null || echo "not exported"
