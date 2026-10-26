#!/usr/bin/env bash
# Hardware-gate build pass: build every board the gate covers.
#
# Four images, cheapest first so a failure surfaces before the expensive compiles:
#   1. asus-c201/forky                        Debian's kernel, nothing compiled
#   2. h96-max-m9/forky                        warm tree
#   3. asus-c201-libreboot/mainline-forky      compiled kernel + fit-sized image
#   4. turing-rk1/forky                        compiled kernel + u-boot
#
# Each build records its outcome beside its artifacts, and gate-verify.sh reads those
# records. So the pass runs to the end rather than stopping at the first failure: a
# board that fails is a finding, not a reason to lose the others.
set -u
cd "$(dirname "$0")"

b2d() { cargo run -q -p boot2deb-cli -- "$@"; }

b2d build --keep-going asus-c201/forky h96-max-m9/forky
# The fitted size is an override, not a recipe value — the shipped recipe keeps its
# hand-picked 2G, and this build measures what it would actually have needed.
b2d build asus-c201-libreboot/mainline-forky --image-size "fit+20%"
b2d build turing-rk1/forky
