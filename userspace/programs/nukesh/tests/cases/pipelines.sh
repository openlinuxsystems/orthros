# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Pipelines, subshells and command substitution.
echo one | tr a-z A-Z
false | true; echo $?; ! true; echo $?
false | true; echo "${PIPESTATUS[@]}"; true | false | true; echo "${PIPESTATUS[*]}"
(set -o pipefail; false | true; echo "pipefail $?")
printf '%s\n' a b c | while read -r line; do echo "got $line"; done
count=0; printf '1\n2\n' | while read -r n; do count=$((count + n)); done; echo "count after pipe $count"
(shopt -s lastpipe; set +m; count=0; printf '1\n2\n' | while read -r n; do count=$((count + n)); done; echo "lastpipe $count")
x=$(printf 'a\nb\n\n\n'); echo "[$x]"
y=`echo backticks`; echo $y
z=$(exit 4); echo "subst status $?"
(cd /tmp && pwd); pwd
echo "$( (echo sub1; echo sub2) | tail -n 1 )"
echo $(( $(printf 'a\nb\n' | wc -l) * 2 ))
seq 1 10 | head -n 3 | tail -n 1
yes | head -n 2
(echo in-subshell; exit 3); echo "subshell status $?"
outer=1; (outer=2; echo "inside $outer"); echo "outside $outer"
echo "$(echo "$(echo "$(echo deep)")")"
v=$(cat <<EOF
from here-doc
EOF
); echo "$v"
echo $(case x in x) echo case-in-subst ;; esac)
{ echo a; echo b; } | { read -r one; read -r two; echo "$two$one"; }
