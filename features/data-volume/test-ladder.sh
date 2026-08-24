#!/bin/sh
set -eu
# Exercise the data-volume first-boot hook's ladder against stubbed block tools.
#
# The property under test is the refusal, not the happy path: this feature exists
# so that reflashing the OS keeps the data disk, which is only true if the hook
# declines to format anything it did not create. Every case below asserts on
# whether `sfdisk`/`mkfs.ext4` were reached at all, because "did not write" is the
# claim — a log message saying it refused proves nothing on its own.
#
# Runs anywhere with /bin/sh; touches no real block device. Run from the boot2deb
# root:  sh features/data-volume/test-ladder.sh

hook=features/data-volume/overlay/etc/boot2deb/first-boot.d/50-data-volume
[ -r "$hook" ] || { echo "run me from the boot2deb root" >&2; exit 2; }

stub="$(mktemp -d)"
trap 'rm -rf "$stub"' EXIT
log="$stub/writes"

# Stubs for everything the hook shells out to. Each reads the scenario from the
# environment, so a case is a set of STUB_* values rather than a fixture tree.
cat > "$stub/findmnt" <<'S'
#!/bin/sh
echo "${STUB_ROOT_SRC:-/dev/mmcblk0p1}"
S
# STUB_DISKS is "name:transport" pairs, so a case can model a board the way
# `lsblk -dno NAME,TRAN` actually reports it -- including a transport-less mmc and
# the read-only eMMC boot partitions, which lsblk lists as whole disks.
cat > "$stub/lsblk" <<'S'
#!/bin/sh
case "$*" in
  *pkname*)         echo "${STUB_ROOT_PK:-mmcblk0}" ;;
  *-dno*NAME,TRAN*) printf '%s' "${STUB_DISKS:-}" | tr ' ' '\n' | grep -v '^$' \
                      | sed 's/:/ /' || true ;;
  *-lno*NAME*)      printf '%s\n%s\n' "nvme0n1" "nvme0n1p1" ;;
  *)                printf '%s' "${STUB_CHILDREN:-nvme0n1}" | tr ' ' '\n' | grep -v '^$' || true ;;
esac
S
cat > "$stub/blkid" <<'S'
#!/bin/sh
case "$*" in
  *-L*)      [ -n "${STUB_LABEL_DEV:-}" ] && echo "$STUB_LABEL_DEV" ;;
  *PTTYPE*)  [ -n "${STUB_PTTYPE:-}" ]    && echo "$STUB_PTTYPE" ;;
  *TYPE*)    [ -n "${STUB_FSTYPE:-}" ]    && echo "$STUB_FSTYPE" ;;
esac
exit 0
S
for c in mountpoint mount mkdir udevadm sleep; do
    printf '#!/bin/sh\nexit 0\n' > "$stub/$c"
done
# The two that must never run except on a blank disk.
printf '#!/bin/sh\necho "sfdisk $*" >> "%s"\n' "$log" > "$stub/sfdisk"
printf '#!/bin/sh\necho "mkfs.ext4 $*" >> "%s"\n' "$log" > "$stub/mkfs.ext4"
chmod +x "$stub"/*

# A copy of the hook reading our config instead of /etc.
sed "s|conf=/etc/boot2deb/data-volumes.conf|conf=$stub/conf|" "$hook" > "$stub/hook"
chmod +x "$stub/hook"
PATH="$stub:$PATH"
export PATH

fails=0
# case <name> <expect: wrote|clean> <conf-line> [STUB_X=y ...]
case_() {
    name="$1"; expect="$2"; conf="$3"; shift 3
    : > "$log"
    printf '%s\n' "$conf" > "$stub/conf"
    env -u STUB_LABEL_DEV -u STUB_PTTYPE -u STUB_FSTYPE \
        -u STUB_DISKS -u STUB_CHILDREN "$@" sh "$stub/hook" > "$stub/out" 2>&1 || true
    if [ "$expect" = clean ] && [ -s "$log" ]; then
        echo "FAIL  $name — wrote to a disk it should not have:"; sed 's/^/        /' "$log"
        fails=$((fails + 1))
    elif [ "$expect" = wrote ] && [ ! -s "$log" ]; then
        echo "FAIL  $name — did not create the volume:"; sed 's/^/        /' "$stub/out"
        fails=$((fails + 1))
    else
        echo "ok    $name"
    fi
}

tab="$(printf '\t')"
vol()      { printf '%s\t%s\text4\t/srv\t%s' "$1" "b2d-data" "${2:-if-blank}"; }
vol_nvme="$(vol nvme)"
vol_never="$(vol nvme never)"
vol_sata="$(vol sata)"
vol_usb="$(vol usb)"
vol_mmc="$(vol mmc)"

# A Turing RK1 as `lsblk` reports it: eMMC holding root, its two read-only boot
# hardware partitions (which lsblk lists as whole disks), and a used NVMe.
rk1="mmcblk0: mmcblk0boot0: mmcblk0boot1: nvme0n1:nvme"

echo "-- the ladder"
case_ "adopts a volume already carrying our label" clean "$vol_nvme" \
    STUB_LABEL_DEV=/dev/nvme0n1p1
case_ "refuses a disk holding a partition table" clean "$vol_nvme" \
    STUB_DISKS=nvme0n1:nvme STUB_PTTYPE=gpt
case_ "refuses a disk holding a bare filesystem" clean "$vol_nvme" \
    STUB_DISKS=nvme0n1:nvme STUB_FSTYPE=ext4
case_ "refuses a disk the kernel already sees partitions on" clean "$vol_nvme" \
    STUB_DISKS=nvme0n1:nvme STUB_CHILDREN="nvme0n1 nvme0n1p1"
case_ "refuses an ambiguous match rather than guessing" clean "$vol_nvme" \
    STUB_DISKS="nvme0n1:nvme nvme1n1:nvme"
case_ "refuses to create under create=never" clean "$vol_never" \
    STUB_DISKS=nvme0n1:nvme
case_ "leaves the mount alone when no disk matches" clean "$vol_nvme" \
    STUB_DISKS=""
case_ "creates on a genuinely blank disk" wrote "$vol_nvme" \
    STUB_DISKS=nvme0n1:nvme STUB_CHILDREN=nvme0n1

echo "-- picking the right disk"
# The case this exists for: sata and usb share the /dev/sd* name, so only the
# transport separates an internal disk from a drive somebody plugged in.
case_ "never writes a USB disk when the volume asks for SATA" clean "$vol_sata" \
    STUB_DISKS=sda:usb STUB_CHILDREN=sda
case_ "never writes a SATA disk when the volume asks for USB" clean "$vol_usb" \
    STUB_DISKS=sda:sata STUB_CHILDREN=sda
case_ "writes the USB disk when that is what was asked for" wrote "$vol_usb" \
    STUB_DISKS=sda:usb STUB_CHILDREN=sda
case_ "skips a /dev/sd* whose transport it cannot read" clean "$vol_sata" \
    STUB_DISKS=sda: STUB_CHILDREN=sda
case_ "never mistakes an NVMe volume for a USB disk" clean "$vol_nvme" \
    STUB_DISKS=sda:usb STUB_CHILDREN=sda
case_ "on a real RK1, an nvme volume finds only the NVMe" clean "$vol_nvme" \
    STUB_DISKS="$rk1" STUB_PTTYPE=gpt
# mmcblk<n>boot<n> are TYPE=disk and read-only; an mmc volume must not see them,
# and mmcblk0 itself is root's disk, so nothing is left to match.
case_ "never touches the eMMC boot hardware partitions" clean "$vol_mmc" \
    STUB_DISKS="$rk1" STUB_CHILDREN=mmcblk0boot0

if [ "$fails" -eq 0 ]; then
    echo "all cases passed"
else
    echo "$fails case(s) failed" >&2
    exit 1
fi
