#!/bin/sh
set -eu
# Exercise boot2deb-install-to against stubbed block tools and an ordinary file that
# stands in for the target disk.
#
# The properties under test are the refusals and the honesty of "done": every case
# asserts on the target file's bytes and on whether the script reported success,
# because "did not write" and "did not claim success" are the claims. A message
# saying it refused proves nothing on its own.
#
# Runs anywhere with /bin/sh, gzip and sha256sum; touches no real block device. Run
# from the boot2deb root:  sh base/test-install-to.sh

script=base/overlay/usr/sbin/boot2deb-install-to
[ -r "$script" ] || { echo "run me from the boot2deb root" >&2; exit 2; }

stub="$(mktemp -d)"
trap 'rm -rf "$stub"' EXIT
mkdir -p "$stub/bin" "$stub/payload" "$stub/sys/block/sdz/holders" "$stub/sys/block/sdz/sdz1/holders" "$stub/dev"
echo 0 > "$stub/sys/block/sdz/ro"

# The image: 1 MiB of structured bytes, so a cut-short or corrupted write cannot
# hash the same.
image="$stub/image.raw"
i=0
: > "$image"
while [ "$i" -lt 256 ]; do printf '%04d-boot2deb-test-image-block-%04d\n' "$i" "$i"; i=$((i + 1)); done > "$stub/block"
i=0
while [ "$i" -lt 64 ]; do cat "$stub/block"; i=$((i + 1)); done > "$image"
image_bytes=$(wc -c < "$image" | tr -d ' ')
image_sha=$(sha256sum < "$image"); image_sha=${image_sha%% *}
gzip -c -n "$image" > "$stub/payload/board.img.gz"
payload_sha=$(sha256sum < "$stub/payload/board.img.gz"); payload_sha=${payload_sha%% *}
write_record() {
    printf '# test record\nfile=board.img.gz\nsha256=%s\nimage_bytes=%s\nimage_sha256=%s\n' \
        "$1" "$image_bytes" "$image_sha" > "$stub/payload/board.img.gz.info"
}

# Stubs for the tools whose answers depend on the machine. Each reads the scenario
# from the environment, so a case is a set of STUB_* values.
cat > "$stub/bin/id" <<'S'
#!/bin/sh
echo 0
S
cat > "$stub/bin/findmnt" <<'S'
#!/bin/sh
[ -z "${STUB_FINDMNT_FAIL:-}" ] || exit 1
echo "${STUB_ROOT_SRC:-/dev/mmcblk0p2}"
S
# lsblk answers the four questions the script asks.
cat > "$stub/bin/lsblk" <<'S'
#!/bin/sh
[ -z "${STUB_LSBLK_FAIL:-}" ] || exit 1
case "$*" in
  *-dno\ TYPE*)          echo "${STUB_TYPE:-disk}" ;;
  *-dno\ MAJ:MIN*)       echo " 8:200" ;;
  *-lnso\ MAJ:MIN,TYPE*) printf '%b' "${STUB_ROOT_CHAIN:-179:2 part\n179:0 disk\n}" ;;
  *-nlo\ MOUNTPOINT*)    printf '%b' "${STUB_MOUNTS:-\n\n}" ;;
  *) exit 2 ;;
esac
S
cat > "$stub/bin/blockdev" <<'S'
#!/bin/sh
case "$1" in
  --getsize64) [ -z "${STUB_SIZE_FAIL:-}" ] || exit 1; echo "${STUB_SIZE:-1073741824}" ;;
  --flushbufs) exit 0 ;;
esac
S
# The decompressor: the real one, or one that emits half the image and fails.
cat > "$stub/bin/zcat" <<'S'
#!/bin/sh
if [ -n "${STUB_DECOMP_FAIL:-}" ]; then
    gzip -dc | head -c 300000
    exit 1
fi
exec gzip -dc
S
# dd: the real one, then optionally a disk that does not keep what it was given.
real_dd=$(command -v dd)
cat > "$stub/bin/dd" <<S
#!/bin/sh
"$real_dd" "\$@" || exit \$?
if [ -n "\${STUB_CORRUPT:-}" ]; then
    printf 'X' | "$real_dd" of="$stub/dev/sdz" bs=1 seek=4096 conv=notrunc 2>/dev/null
fi
S
chmod +x "$stub/bin"/*

# A copy of the script reading the stub tree instead of the machine's, and accepting
# an ordinary file where it requires a block device.
sed -e "s|^payload_dir=.*|payload_dir=$stub/payload|" \
    -e "s|^sysblock=.*|sysblock=$stub/sys/block|" \
    -e "s|^devdir=.*|devdir=$stub/dev|" \
    -e 's|\[ -b "\$dev" \]|[ -f "$dev" ]|' \
    "$script" > "$stub/install-to"

fails=0
# case_ <name> <expect: installed|untouched|failed> <output pattern> [STUB_X=y ...]
#   installed: exit 0, "done:" reported, the target holds the image.
#   untouched: non-zero exit, no "done:", the target holds what it held before.
#   failed:    non-zero exit and no "done:"; the target may hold a partial image.
# The pattern must match the output, so a case passes only for its own reason and
# not because an earlier check happened to refuse.
case_() {
    name="$1"; expect="$2"; pattern="$3"; shift 3
    printf 'OLD-DISK-CONTENTS' > "$stub/dev/sdz"
    truncate -s 2M "$stub/dev/sdz"
    before=$(sha256sum < "$stub/dev/sdz")
    rc=0
    printf 'sdz\n' | env PATH="$stub/bin:$PATH" "$@" sh "$stub/install-to" "$stub/dev/sdz" \
        > "$stub/out" 2>&1 || rc=$?
    after=$(sha256sum < "$stub/dev/sdz")
    written=$(head -c "$image_bytes" "$stub/dev/sdz" | sha256sum); written=${written%% *}
    ok=yes
    case "$expect" in
        installed)
            [ "$rc" -eq 0 ] && grep -q '^done:' "$stub/out" && [ "$written" = "$image_sha" ] || ok=no ;;
        untouched)
            [ "$rc" -ne 0 ] && ! grep -q '^done:' "$stub/out" && [ "$before" = "$after" ] || ok=no ;;
        failed)
            [ "$rc" -ne 0 ] && ! grep -q '^done:' "$stub/out" || ok=no ;;
    esac
    grep -q -- "$pattern" "$stub/out" || ok=no
    if [ "$ok" = yes ]; then
        echo "ok    $name"
    else
        echo "FAIL  $name (exit $rc)"; sed 's/^/        /' "$stub/out"
        fails=$((fails + 1))
    fi
}

write_record "$payload_sha"
echo "-- the write"
case_ "installs and verifies a good payload on a good disk" installed 'holds the image, verified'
case_ "a decompressor failing after output is not reported done" failed 'failed part way' \
    STUB_DECOMP_FAIL=1
case_ "a disk that does not keep the bytes fails the read-back" failed \
    'does not hold the image after the write' STUB_CORRUPT=1

echo "-- refusals before the target is touched"
case_ "refuses a disk smaller than the image" untouched 'and the image needs' STUB_SIZE=4096
case_ "refuses when the disk size cannot be read" untouched 'cannot read the size' \
    STUB_SIZE_FAIL=1
case_ "refuses when lsblk cannot answer" untouched 'cannot ask lsblk' STUB_LSBLK_FAIL=1
case_ "refuses a partition" untouched "not a whole disk" STUB_TYPE=part
case_ "refuses when the root's source cannot be found" untouched \
    'cannot establish what / is mounted from' STUB_FINDMNT_FAIL=1
case_ "refuses when the root's disk cannot be established" untouched \
    'cannot establish which disk' STUB_ROOT_CHAIN='253:0 crypt\n'
case_ "refuses the disk the root sits on, through a mapper layer" untouched \
    'the disk this system is running from' \
    STUB_ROOT_CHAIN='253:0 crypt\n8:201 part\n8:200 disk\n'
case_ "refuses a disk with a mounted partition" untouched '/mnt/data mounted' \
    STUB_MOUNTS='\n/mnt/data\n'
touch "$stub/sys/block/sdz/sdz1/holders/dm-3"
case_ "refuses a disk a device holds" untouched 'in use by dm-3'
rm "$stub/sys/block/sdz/sdz1/holders/dm-3"
echo 1 > "$stub/sys/block/sdz/ro"
case_ "refuses a read-only disk" untouched 'is read-only'
echo 0 > "$stub/sys/block/sdz/ro"

echo "-- the payload and its record"
write_record "$(printf '%064d' 0)"
case_ "refuses a payload that does not match its record" untouched 'does not match its record'
rm "$stub/payload/board.img.gz.info"
case_ "refuses a payload with no record" untouched 'no record beside'
printf 'file=board.img.gz\nsha256=nothex\nimage_bytes=1\nimage_sha256=%s\n' "$image_sha" \
    > "$stub/payload/board.img.gz.info"
case_ "refuses a malformed record" untouched 'no valid sha256 for the payload'
write_record "$payload_sha"
case_ "installs again once the record is whole" installed 'holds the image, verified'

if [ "$fails" -eq 0 ]; then
    echo "all cases passed"
else
    echo "$fails case(s) failed" >&2
    exit 1
fi
