#!/bin/bash
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Pipes, redirections, process substitution, background jobs.
tmp=/tmp/nukesh-s3.$$
mkdir -p "$tmp"
printf 'b\na\nc\n' >"$tmp/in"
sort <"$tmp/in" >"$tmp/out"
cat "$tmp/out"
cat <(sort "$tmp/in") | while read -r l; do echo "sorted $l"; done
echo err 2>&1 >/dev/null | cat
{ echo one; echo two >&2; } 2>"$tmp/err" >"$tmp/std"
echo "std: $(cat "$tmp/std") err: $(cat "$tmp/err")"
exec {fd}>"$tmp/named"
echo "via named fd" >&$fd
exec {fd}>&-
cat "$tmp/named"
echo hi &>"$tmp/both"
cat "$tmp/both"
cat <<<"here string" | wc -c
for i in 1 2 3; do
	(sleep 0.$i; echo "job $i") &
done
wait
echo "all jobs done"
sleep 5 &
pid=$!
kill $pid
wait $pid
echo "killed status $?"
true | false | true
echo "pipestatus ${PIPESTATUS[*]}"
x=$(printf 'a\nb\n\n\n')
echo "[$x]"
read -r first < "$tmp/in"
echo "first=$first"
while IFS= read -r line; do
	echo "line=$line"
done < <(printf 'x\ny\n')
coproc_test=$( (echo sub1; echo sub2) | tail -1 )
echo "$coproc_test"
echo $(( $(wc -l <"$tmp/in") * 2 ))
set +e
cd "$tmp" && ls | sort
cd / || exit 1
rm -r "$tmp"
[ -d "$tmp" ] || echo "cleaned"
