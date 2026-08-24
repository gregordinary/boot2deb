#!/usr/bin/env bash
# Off-board verification for the hardware-gate pass: everything checkable without a
# board. The boot itself is the part only hardware can answer.
#
# The checks themselves are `boot2deb verify-image`, so they are Rust that reads the
# GPT and the ext4 superblock with the code that *wrote* them, and are covered by that
# crate's tests. What is left here is the gate's own job: which recipes this pass
# covered, whether each one's build actually ran, and one exit status for all of them.
set -u
cd "$(dirname "$0")"

fail=0

# The recipes this pass built, each paired with the gate-pass log name that records
# whether its build succeeded.
while read -r recipe logname; do
    [ -z "$recipe" ] && continue
    echo
    echo "########## $recipe ##########"
    # A build that failed has nothing to verify, and saying so beats reporting a
    # missing artifact as if the artifact were the problem.
    if [ ! -f "gate-logs/$logname.exit" ]; then
        echo "  FAIL           the build did not run (no gate-logs/$logname.exit)"
        fail=1
        continue
    fi
    status=$(cat "gate-logs/$logname.exit")
    if [ "$status" != 0 ]; then
        echo "  FAIL           the build exited $status; see gate-logs/$logname.log"
        fail=1
        continue
    fi
    cargo run -q -p boot2deb-cli -- verify-image "$recipe" || fail=1
done <<'RECIPES'
asus-c201/forky c201-forky
h96-max-m9/forky h96-forky
asus-c201-libreboot/mainline-forky c201-libreboot-fit
turing-rk1/forky rk1-forky
RECIPES

echo
[ "$fail" = 0 ] && echo "OFF-BOARD VERIFICATION: all checks passed" \
                || echo "OFF-BOARD VERIFICATION: failures above"
exit "$fail"
