# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# printf, read and mapfile where they differ from C's or a simple reading.
# printf: output longer than any fixed buffer, numbers past 64 bits as C
# clamps them, negative `*` widths and precisions, byte widths.
printf -v v '%600d' 1; echo "${#v}"
printf -v v '%.600f' 1; echo "${#v}"
printf '%d %d\n' 99999999999999999999 -99999999999999999999 2>/dev/null; echo "status $?"
printf '%u %x %i %o\n' -1 18446744073709551616 9223372036854775808 -1 2>/dev/null
printf '%d\n' 12x 2>/dev/null; echo "status $?"
printf '%.*f|%.*s|%*d|%.*d|\n' -1 3.14159 -2 abc -4 7 3 5
printf '%3s|%-4s|%3b|\n' é é é
printf -v v 'a%cb' ''; echo "${#v}"
printf '%q %q %q %q %q\n' =a a=b '~x' 'x~' ']'
printf '%.2Q|%.*Q|%5.1Q|\n' 'a b c' 1 'x y' 'x y'
printf '%d\n' 1 x 2 2>&1 | sed 's/^.*printf: /printf: /'
printf -v 1x a 2>/dev/null; echo "status $?"
printf -v 'a[' a 2>/dev/null; echo "status $?"
printf -v 'arr[1+1]' z; echo "${arr[2]}"
# An arithmetic error in the subscript abandons the command line.
f() { printf -v 'arr[x y]' z; echo "not here"; }; f 2>/dev/null; echo "not here"
echo "status $?"
# read: the last name's trailing delimiter, NUL bytes, names checked as
# they are set, timeouts.
IFS=: read a b <<< "1:2:"; echo "[$a][$b]"
IFS=": " read a b <<< "1 : 2 : 3 : "; echo "[$a][$b]"
IFS=: read a b <<< "1:2::"; echo "[$a][$b]"
IFS=": " read a b <<< "1 :: "; echo "[$a][$b]"
printf 'a\0b\nc\n' | { read x; echo "[$x]"; }
printf 'a\0bc' | { read -n 2 x; echo "[$x]"; }
read u 'v w' <<< 'a b' 2>/dev/null; echo "status $? u=$u"
printf 'a\nb\n' | { read 1x 2>/dev/null; read y; echo "y=$y"; }
read -t inf x <<< y 2>/dev/null; echo "status $?"
read -t +.5 x <<< y; echo "status $? $x"
read -u x x 2>/dev/null; echo "status $?"
# mapfile: bad counts, a read-only or associative array, NUL bytes.
mapfile -c 0 m < /dev/null 2>/dev/null; echo "status $?"
mapfile -n -1 m < /dev/null 2>/dev/null; echo "status $?"
readonly PRO=1; mapfile PRO < /dev/null 2>/dev/null; echo "status $?"
declare -A H; mapfile H < /dev/null 2>/dev/null; echo "status $?"
printf 'a\0b\0' | { mapfile -d '' w; echo "${#w[@]} ${w[0]} ${w[1]}"; }
printf 'a\0b\nc' | { mapfile -t w; echo "${#w[@]} ${w[0]} ${w[1]}"; }
# echo -e takes octal only after a 0, printf %b with or without.
echo -e '\101|\0101|\1|\0102'; printf '%b|%b|%b\n' '\101' '\0101' '\1012'
