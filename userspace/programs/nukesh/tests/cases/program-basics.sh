#!/bin/sh
# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Here-documents, functions, loops and traps.
greet() {
	cat <<EOF
Hello, $1!
  Today is $(echo day) and 2+2=$((2 + 2))
EOF
}
greet world
cat <<'END' | tr a-z A-Z
quoted $HOME `date`
END
count=0
while read -r name value; do
	count=$((count + 1))
	echo "$count: $name=$value"
done <<DATA
alpha 1
beta 2
gamma 3
DATA
echo "count=$count"
trap 'echo "exit trap, status $?"' EXIT
trap 'echo caught USR1' USR1
kill -USR1 $$
for f in a b c; do
	case $f in
	a) echo "first" ;;
	b) continue ;;
	*) echo "other $f" ;;
	esac
done
i=0
while :; do
	i=$((i + 1))
	[ $i -ge 5 ] && break
done
echo "i=$i"
outer() {
	inner() { echo "inner $1"; }
	inner "$@"
}
outer x y
echo "line $LINENO"
(
	cd /tmp || exit
	echo "subshell in $(pwd)"
	exit 3
)
echo "subshell status $?"
x=$(exit 4)
echo "subst status $?"
set -- one two three
while [ $# -gt 0 ]; do
	echo "arg: $1"
	shift
done
echo ${UNSET_THING:-default} ${#}
false
