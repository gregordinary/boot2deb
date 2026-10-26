#!/usr/bin/env bash
# Off-board verification for the hardware-gate pass: everything checkable without a
# board. The boot itself is the part only hardware can answer.
#
# The checks are `boot2deb verify-image`, so they are Rust that reads the GPT and the
# ext4 superblock with the code that *wrote* them, and are covered by that crate's
# tests. It reads each point's build record first, so a board whose newest build failed
# or never finished is reported as that, rather than as whatever its older artifacts
# show. Where a recipe commits an outputs record (`recipes/<recipe>.outputs`),
# verify-image also holds the build to the bytes it names for this build host. One exit
# status covers every point.
set -u
cd "$(dirname "$0")"

exec cargo run -q -p boot2deb-cli -- verify-image \
    asus-c201/forky \
    h96-max-m9/forky \
    asus-c201-libreboot/mainline-forky \
    turing-rk1/forky
