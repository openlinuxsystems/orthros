# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Redirections, here-documents and descriptors.
tmp=/tmp/nukesh-redir.$$
mkdir -p "$tmp"
{ echo a; echo b; } > "$tmp/f"; cat "$tmp/f"; echo c >> "$tmp/f"; wc -l < "$tmp/f"
exec 3>"$tmp/g"; echo fd3 >&3; exec 3>&-; cat "$tmp/g"
x=$( { echo out; echo err >&2; } 2>&1 ); echo "$x"
echo err 2>&1 >/dev/null | cat
{ echo one; echo two >&2; } 2>"$tmp/err" >"$tmp/std"; echo "std: $(cat "$tmp/std") err: $(cat "$tmp/err")"
exec {fd}>"$tmp/named"; echo "via named fd" >&$fd; exec {fd}>&-; cat "$tmp/named"
echo hi &>"$tmp/both"; cat "$tmp/both"; echo more &>>"$tmp/both"; cat "$tmp/both"
echo text >"$tmp/rw"; exec 4<>"$tmp/rw"; read -r line <&4; echo "read $line"; exec 4<&-
(set -C; echo x > "$tmp/rw") 2>/dev/null || echo noclobber; (set -C; echo y >| "$tmp/rw"; cat "$tmp/rw")
cat <<<"here string" | wc -c
read a b <<< "x y z"; echo "$a|$b"
cat <<EOF
Hello, ${USER_NAME:-world}!
  2+2=$((2 + 2)) and $(echo sub) and \$literal and \`tick\`
EOF
cat <<'END' | tr a-z A-Z
quoted $HOME `date` \n
END
cat <<-TABS
	tab-indented
		twice
	TABS
cat <<A; cat <<B
first
A
second
B
while read -r name value; do echo "$name=$value"; done <<DATA
alpha 1
beta 2
DATA
f() { cat; } <<EOF
function body input
EOF
f
echo "in a pipe" | { read -r v; echo "got: $v"; }
printf 'b\na\nc\n' > "$tmp/in"; sort < "$tmp/in" > "$tmp/out"; cat "$tmp/out"
read -r first < "$tmp/in"; echo "first=$first"
cat < "$tmp/in" | sort -r | head -n 1
exec 5>&1; echo "to five" >&5; exec 5>&-
{ echo stdout; echo stderr >&2; } 2>&1 | sort
echo nothing > /dev/null; cat /dev/null; echo null-ok
>"$tmp/empty"; [ -s "$tmp/empty" ] || echo empty-made
cd "$tmp" && ls | sort; cd /
rm -r "$tmp"; [ -d "$tmp" ] || echo cleaned
