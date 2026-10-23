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
