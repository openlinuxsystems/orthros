# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Syntax errors in bash's words. Each text is read by eval; its messages
# are shown from their line numbers on, without the script's name (and
# for an error running it, at the script's line).
check() {
  local out line
  out=$(eval "$1" 2>&1)
  echo "[$1] $?"
  while IFS= read -r line; do
    line=${line#*: eval: }
    [[ -n $line ]] && echo "  ${line#*syntax.sh: }"
  done <<< "$out"
}
# `[[ ]]`: newlines where a term starts and after a whole one.
check $'[[\na ]] && echo yes'
check $'[[ a &&\n b ]] && echo yes'
check $'[[ ( a == b\n) ]] && echo yes'
check $'[[ -f /\n]] && echo yes'
check $'[[ a\n]]'
check $'[[ ( a\n== b ) ]]'
check $'[[ a ==\n b ]]'
check $'[[ -f\n/ ]]'
# A term that cannot start, or a word with no operator.
check '[[ ]]'
check '[[ ! ]]'
check '[[ a && ]]'
check '[[ && a ]]'
check '[[ || a ]]'
check '[[ ( ) ]]'
check '[[ & ]]'
check '[[ < a ]]'
check '[[ ;; ]]'
check '[[ a b ]]'
check '[[ a ; ]]'
check '[[ a | b ]]'
check '[[ a << b ]]'
check '[[ a |& b ]]'
check '[[ a ]]]'
check '[[ a !=b ]]'
check '[[ a -a b ]]'
check '[[ a <(echo) ]]'
# Operands.
check '[[ -f ]]'
check '[[ -f && b ]]'
check '[[ ( -f ) ]]'
check '[[ a == ]]'
check '[[ a == && b ]]'
check '[[ a < (b) ]]'
check '[[ a == (b|c) ]]'
check '[[ a -eq ]]'
check '[[ a =~ ]]'
check '[[ -n == -n ]]'
check '[[ -f == x ]]'
check '[[ x == -n ]] && echo yes'
check '[[ -a == ]] && echo yes'
check '[[ -ef ]] && echo yes'
# After a whole term.
check '[[ a == b c ]]'
check '[[ a ) ]]'
check '[[ a == b ; ]]'
check '[[ ( a ) ( b ) ]]'
check '[[ a == b > c ]]'
check '[[ -z a b ]]'
check '[[ ( a ) == b ]]'
check $'[[ a == b\n\n) ]]'
# Parentheses.
check '[[ ( a ]]'
check '[[ ( a == b ; ]]'
check '[[ ( -f a b ) ]]'
check '[[ ( a b ) ]]'
check $'[[ (\n( a b'
check '[[ ( ( a ) ) ]] && echo yes'
# Regular expressions: groups may hold blanks and newlines; one ends at a
# metacharacter outside its groups, and may be empty.
check $'[[ $\'a\\nb\' =~ (a\nb) ]] && echo yes'
check '[[ a =~ x|a ]] && echo yes'
check '[[ a =~ && b ]] && echo yes'
check '[[ a =~ || ]] && echo yes'
check '[[ a =~ ; ]]'
check '[[ a =~ a) ]]'
check '[[ a =~ <a> ]]'
check '[[ a =~ [&] ]]'
# The end of the text.
check '[[ a &&'
check '[[ ( ('
check '[[ ( ( a == b'
check '[[ a == b'
check '[[ a =='
check '[[ -f'
check '[[ a =~ ( ]]'
# Only a separator lets a command follow a compound one on its line.
check '{ echo a; } b'
check 'if true; then :; fi b'
check '(echo a) b'
check '[[ a ]] b'
check 'for i in 1; do :; done b'
check 'f() { :; } b'
check '{ :; } >/dev/null b'
check '[[ a ]] (b)'
check '{ :; } !'
check '{ echo a; }; echo b & wait'
# A loop's variable that is not a name fails the loop as it starts.
check 'for 1x in a; do echo in; done'
check 'select a-b in x; do break; done'
check 'for a[1] in $(echo x >&2); do echo in; done'
check 'for x y in 1; do :; done'
# `[[ ]]`'s parentheses show as written.
f() { [[ ( a ) && ( b || ! ( c ) ) ]]; [[ ! a == b ]]; }
declare -f f
# A subscript at a command's start runs to its `]`, over lines too; a
# command's line is where the token after its first word ends.
check $'a[1\n+ 1]=x; declare -p a'
check 'b[1 b'
check $'nosuch1 \\\n x'
check $'"no\nsuch2" x'
check 'c[1]=2 echo hi'
check 'd[$(echo s >&2)]=$(echo v >&2) true'
check 'e[1 b] x'
check $'echo $LINENO \\\n $LINENO'
check $'x=1 \\\n nosuch3'
check $'cd \'/nonexistent-dir\nx\''
check 'nosuch4 >/nonexistent-dir/x'
check 'declare -p nosuch5'
# In an expanded here-document a `\` at the end of a line joins the next
# to it before the delimiter is looked for.
cat <<EOF
a\
EOF
b
EOF
cat <<'EOF'
c\
EOF
