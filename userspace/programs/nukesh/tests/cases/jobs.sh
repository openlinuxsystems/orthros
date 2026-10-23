# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Background jobs and waiting for them.
sleep 0.1 & wait $!; echo "waited $?"
(exit 7) & wait $!; echo "status $?"
for i in 1 2 3; do (echo "job $i") & wait; done; echo "all jobs done"
(sleep 0.2; exit 3) & a=$!; (exit 4) & b=$!; wait $a; echo "a $?"; wait $b; echo "b $?"
(sleep 1; exit 2) & (exit 1) & wait -n; echo "first $?"; wait
sleep 5 & pid=$!; kill $pid; wait $pid; echo "killed $?"
sleep 5 & kill %1; wait %1; echo "by job $?"
wait; echo "nothing to wait for $?"
wait 99999 2>/dev/null; echo "not a child $?"
echo "$!" | while read -r p; do [ -n "$p" ] && echo has-last-pid; done
{ echo grouped; } & wait
x=outer; { x=inner; } & wait; echo $x
