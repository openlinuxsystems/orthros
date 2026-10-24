# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# A bad substitution is an error when it is expanded, not when it is
# read, and it ends the script, as in dash.
# An assignment to a read-only variable before a command ends it too.
(readonly ro=1; ro=2 true; echo "not reached") 2>/dev/null; echo "read-only $?"
(readonly ro=1; ro=2 printenv ro; echo "not reached") 2>/dev/null; echo "read-only $?"
# Status operands from 0 to 2^31 - 1, as dash takes them.
(exit -1); echo "exit -1: $?"
(exit 2147483648); echo "exit 2^31: $?"
(exit 2147483647); echo "exit 2^31-1: $?"
if [ -n "$BASH_VERSION" ]; then y=${x/a/b} z=${x^^}; fi; echo "untaken"
f() { echo "in f"; echo ${x/a/b}; }
echo "f defined"
f
echo "not reached"
