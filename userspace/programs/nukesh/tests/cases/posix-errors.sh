# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# A bad substitution is an error when it is expanded, not when it is
# read, and it ends the script, as in dash.
if [ -n "$BASH_VERSION" ]; then y=${x/a/b} z=${x^^}; fi; echo "untaken"
f() { echo "in f"; echo ${x/a/b}; }
echo "f defined"
f
echo "not reached"
