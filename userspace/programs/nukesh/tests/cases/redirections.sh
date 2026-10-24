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
# set -C makes a file that is not there with O_EXCL: a dangling link is
# not followed, and of shells racing for a lock only one takes it.
(set -C; ln -s "$tmp/none" "$tmp/dangling"; echo z > "$tmp/dangling") 2>/dev/null || echo "noclobber dangling"
[ -e "$tmp/none" ] || echo "none made"; (set -C; echo z > /dev/null && echo "device written")
for i in 1 2 3 4 5 6 7 8; do (set -C; echo "$i" > "$tmp/lock") 2>/dev/null && echo took & done; wait
# A compound command's redirection fails at the line the command ends on.
{
	if true; then
		:
	fi < "$tmp/missing"
	{ :; } 2>/dev/null \
		< "$tmp/missing"
	while false; do :; done < "$tmp/missing"
} 2>&1 | sed "s|^[^ ]* ||; s|$tmp|TMP|"
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
# Outside POSIX mode a redirection's word is split and globbed as an
# argument is, and anything but one word is an ambiguous redirect; the
# command's own assignments are not seen. In POSIX mode neither happens.
(
	cd "$tmp" && touch p1 p2 q1
	x="a b"; echo one > $x; echo "s=$?"
	set -- u v; echo two > "$@"; echo "s=$?"
	echo three > p*; echo "s=$?"; echo four > q*; cat q1
	x="  e  "; echo five > $x; cat e
	x=; echo six > $x; echo "s=$?"
	echo seven > {g,h}; echo "s=$?"
	x="a b"; x=1 echo eight > $x; echo "s=$?"
	f() { cat; }; x=a; x=1 f <<-EOF
	here $x
	EOF
	x="a b"; y=$(< $x); echo "s=$?"
	set -o posix; x="a b"; echo nine > $x; cat "a b"; echo ten > p*; cat "p*"
) 2>&1 | sed 's/^[^ ]* line [0-9]*: //'
# After `>&` or `<&`, a `-` stands alone and closes the descriptor.
echo a 2>&-x; echo b >&- 1 2>/dev/null; echo "s=$?"
# A descriptor that is not open is named as bash names it: by its number
# where written as one, by the descriptor redirected for a move, by the
# word as written otherwise, unless another descriptor is redirected.
(
	fd=7; echo a >&$fd; echo b 2>&$fd; echo c >&7; echo d >&$fd-; echo e 3>&$fd-
	x=; echo f >&$x; echo g >&""; echo h 2>&""; cat >&$fd- < /dev/null
	x="1 2"; echo i 2>&$x-; echo "s=$?"
) 2>&1 | sed 's/^[^ ]* line [0-9]*: //; s/^[^ ]*: redirection/redirection/'
# `{name[subscript]}` takes an element for the new descriptor.
exec {fds[1]}>"$tmp/element"; echo "to an element" >&${fds[1]}; exec {fds[1]}>&-; cat "$tmp/element"
i=1; exec {more[i+1]}>&1; echo "keys ${!more[@]}"; exec {more[2]}>&-
(exec {r[@]}>/dev/null) 2>&1 | sed 's/^[^ ]* line [0-9]*: //'
# exec's failed redirection ends only a POSIX script.
{ exec 3>"$tmp/no/such"; } 2>/dev/null; echo "went on $?"
cd "$tmp" && ls | sort; cd /
rm -r "$tmp"; [ -d "$tmp" ] || echo cleaned
