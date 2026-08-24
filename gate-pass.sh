#!/usr/bin/env bash
# Hardware-gate build pass: build every board the gate covers, from one command.
#
# Four images, cheapest first so a failure surfaces before the expensive compiles:
#   1. asus-c201/forky                        Debian's kernel, nothing compiled
#   2. h96-max-m9/forky                        warm tree
#   3. asus-c201-libreboot/mainline-forky      compiled kernel + fit-sized image (WP8)
#   4. turing-rk1/forky                        compiled kernel + u-boot
#
# Each build's log is its own file; the exit status is recorded beside it so a later
# pass can tell "not run" from "failed". Runs to the end rather than stopping at the
# first failure — a board that fails is a finding, not a reason to lose the other three.
set -u
cd "$(dirname "$0")"

LOGS=gate-logs
mkdir -p "$LOGS"

run() {
    local name=$1
    shift
    echo "=== $(date -Is)  START  $name ==="
    cargo run -q -p boot2deb-cli -- build "$@" >"$LOGS/$name.log" 2>&1
    local status=$?
    echo "$status" >"$LOGS/$name.exit"
    echo "=== $(date -Is)  END    $name (exit $status) ==="
}

run c201-forky            asus-c201/forky
run h96-forky             h96-max-m9/forky
# WP8: the fitted size is an override, not a recipe value — the shipped recipe keeps
# its hand-picked 2G, and this build measures what it would actually have needed.
run c201-libreboot-fit    asus-c201-libreboot/mainline-forky --image-size "fit+20%"
run rk1-forky             turing-rk1/forky

echo
echo "=== $(date -Is)  PASS COMPLETE ==="
for f in "$LOGS"/*.exit; do
    printf '%-24s exit %s\n' "$(basename "$f" .exit)" "$(cat "$f")"
done
