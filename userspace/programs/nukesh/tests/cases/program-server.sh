#!/bin/bash
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Bash features a server script uses.
set -euo pipefail
shopt -s nullglob
declare -A config=([host]=localhost [port]=8080)
log() { printf '[%s] %s\n' "$1" "${*:2}"; }
log INFO "starting on ${config[host]}:${config[port]}"
services=(web db cache)
for s in "${services[@]}"; do
	log INFO "service $s"
done
check() {
	local name=$1
	if [[ $name == d* ]]; then
		return 0
	fi
	return 1
}
for s in "${services[@]}"; do
	if check "$s"; then log OK "$s"; else log SKIP "$s"; fi
done
mapfile -t nums < <(seq 1 5)
total=0
for n in "${nums[@]}"; do ((total += n)); done
echo "total=$total"
parse_args() {
	local OPTIND opt verbose=0 file=
	while getopts ":vf:" opt; do
		case $opt in
		v) verbose=1 ;;
		f) file=$OPTARG ;;
		:) echo "missing arg for -$OPTARG"; return 1 ;;
		\?) echo "bad option -$OPTARG"; return 1 ;;
		esac
	done
	shift $((OPTIND - 1))
	echo "verbose=$verbose file=$file rest=$*"
}
parse_args -v -f conf.txt a b
parse_args -x || echo "parse failed"
parse_args -f || echo "parse failed 2"
str="key1=val1;key2=val2"
IFS=';' read -ra pairs <<<"$str"
for p in "${pairs[@]}"; do
	echo "${p%%=*} -> ${p#*=}"
done
cleanup() { echo "cleanup ran"; }
trap cleanup EXIT
err() { echo "ERR trap at line $1"; }
trap 'err $LINENO' ERR
result=$(printf '%s,' "${services[@]}")
echo "${result%,}"
printf -v padded '%-8s|' "${services[@]}"
echo "$padded"
declare -i n=10
n=n*2+1
echo $n
lower=${config[host]^^}
echo "$lower"
[[ -v config[host] ]] && echo "host is set"
[[ -v config[nope] ]] || echo "nope is not set"
arr=()
arr+=("a b")
arr+=(c)
echo "${#arr[@]}: ${arr[0]}"
echo "done"
false
echo "not reached"
