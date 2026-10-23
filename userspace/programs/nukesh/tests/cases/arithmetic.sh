# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Arithmetic.
echo $(( 2**10 )) $(( 7/2 )) $(( 7%3 )) $(( 1<<4 )) $(( (3>2) && (1||0) )) $(( 0x1f )) $(( 8#17 )) $(( -5 / 2 )) $(( -7 % 3 ))
echo $((16#ff)) $((2#1010)) $((36#z)) $((017)) $((1 + 2 * 3)) $(( (1+2) * 3 ))
echo $((10 > 5 ? 1 : 0)) $((5 == 5 ? 7 : 8)) $((~0)) $((!5)) $((3 ^ 5)) $((6 & 3)) $((6 | 3)) $((-8 >> 1))
echo $(( 9223372036854775807 + 1 )) $(( -9223372036854775807 - 1 ))
x=5; let 'x += 3' 'y = x * 2'; echo $x $y
x=10; ((x *= 2, x -= 5)); echo $x; ((x++)); ((++x)); echo $x $((x--)) $x
i=0; while (( i < 3 )); do echo $i; ((i++)); done
for ((j=0; j<3; j++)); do printf '%d ' $j; done; echo
for ((;;)); do break; done; echo forever-broken
a=3 b=4; echo $(( a*a + b*b )) $((a+=1)) $a
v=x; x=7; echo $((v)) $(( v + 1 ))
echo $(( $(echo 3) + 4 ))
declare -i n=10; n=n*2+1; echo $n; n+=4; echo $n
(( 0 )); echo "zero gives $?"; (( 2 )); echo "two gives $?"
let 'z = 0'; echo "let zero $?"
echo $(( 1, 2, 3 ))
(echo $(( 1 / 0 ))) 2>/dev/null || echo "division by zero failed"
f() { (( $1 > 1 )) && echo big || echo small; }; f 2; f 0
n=0; for w in $(seq 1 5); do n=$((n+w)); done; echo $n
printf '%d %d %d\n' "'A" 0x10 010
