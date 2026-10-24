# SPDX-FileCopyrightText: 2026 The Dukovo contributors
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Traps, errexit and nounset.
(trap 'echo trapped' EXIT; echo before)
(trap 'echo "exit trap, status $?"' EXIT; exit 5); echo "after $?"
(trap 'echo caught USR1' USR1; kill -USR1 $BASHPID; echo after-usr1)
(trap 'echo int' INT; kill -INT $BASHPID; echo after-int)
(trap '' TERM; kill -TERM $BASHPID; echo ignored-term)
(trap 'echo first' EXIT; trap 'echo second' EXIT; true)
(trap 'echo cleared' EXIT; trap - EXIT; echo none)
(trap 'echo "err at $LINENO"' ERR; false; echo after-err)
(trap 'echo ret' RETURN; f() { :; }; f)
(trap -p EXIT; trap 'echo x' EXIT; trap -p EXIT; trap - EXIT)
(set -e; false || echo ok; (false); echo notreached); echo "errexit status $?"
(set -e; if false; then :; fi; false && true; ! true; echo survived)
(set -e; f() { false; echo "in f"; }; f || true; echo "f ok")
(set -e; x=$(false); echo "not here"); echo "assignment status $?"
(set -e; false | true; echo pipe-ok)
(set -eo pipefail; false | true; echo "not here"); echo "pipefail errexit $?"
(set -u; echo ${undefined_var-ok}; echo $undefined_var2; echo not-here) 2>/dev/null; echo "nounset status $?"
(set -u; a=(); echo "${a[@]-}empty")
(set -e; trap 'echo "exited $?"' EXIT; false)
(sleep 5 & pid=$!; kill $pid; wait $pid; echo "killed status $?")
(trap 'echo child-exit' EXIT; (exit 1); echo "inner status $?")
(trap 'false; exit' EXIT; true); echo "exit in the EXIT trap: $?"
(f() { false; exit; }; trap f EXIT; true); echo "exit in the EXIT trap's function: $?"
(trap 'false; exit' USR1; kill -USR1 $BASHPID; echo not-here); echo "exit in a signal's trap: $?"
(trap '(false; exit); echo "subshell $?"' EXIT; true)
