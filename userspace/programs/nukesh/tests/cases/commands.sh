# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# type, command, hash, ulimit, umask, kill -l, wait and cd where bash has
# its own ways.
# Looking a command up hashes nothing; a directory is no command; aliases
# count only where they are expanded.
hash -r; hash; type sed > /dev/null; command -v sed > /dev/null; hash
hash -p /nowhere/ls ls; type ls; hash -t ls; hash -r
command -v /; echo "status $?"
alias ll='ls -l'; type -t ll; echo "status $?"
shopt -s expand_aliases; type -t ll; command -v ll; shopt -u expand_aliases
# Several resources are shown with their descriptions.
ulimit -c -n | sed 's/) .*/) N/'
ulimit -n -H > /dev/null; echo "status $?"
ulimit -S -n '2*100' 2>/dev/null; echo "status $?"
# umask's errors name the character that is wrong.
umask 999 2>&1 | sed 's/^.*umask: /umask: /'
umask u=rwz 2>&1 | sed 's/^.*umask: /umask: /'
(umask 022; umask o=,g=w; umask)
# Signals: bash's table, exit statuses as names, real-time names.
kill -l | sed -n 1,6p
kill -l 9 137 0 EXIT
trap 'echo rt' RTMIN+3; trap -p RTMIN+3; trap - RTMIN+3
# wait takes each operand in turn.
wait x 2>/dev/null; echo "status $?"
wait %9 2>/dev/null; echo "status $?"
wait -n 99999 2>/dev/null; echo "status $?"
# cd with PWD read-only still changes directory; OLDPWD is exported.
(readonly PWD; cd /; echo "status $?"; cd /tmp; echo "in $(pwd -P)") 2>/dev/null
cd /tmp; export -p | sed -n '/OLDPWD/s/=.*//p'; cd - > /dev/null
[ -n "$BASH" ] && echo "BASH set"
# set -k: assignments after the name go to the command's environment too.
set -k; g() { echo "g:$#:$*:[$k]"; }; g k=1 a k=2 b; echo e=6 "q=1" q\=2; set +k; echo e=6
