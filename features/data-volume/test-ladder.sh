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
# The stubbed world defaults to a Turing RK1 whose NVMe is blank, and the first case
# proves the hook creates the volume there. Every refusing case then changes one
# thing about that world, so a refusal is caused by the thing named and cannot pass
# because the scenario was broken. Where a case names a reason, the hook's log must
# contain it too.
#
# The hook runs with a PATH of the stubs plus a few real text tools and nothing
# else, so removing a stub makes that tool absent rather than revealing the host's.
#
# Runs anywhere with /bin/sh; touches no real block device. Run from the boot2deb
# root:  sh features/data-volume/test-ladder.sh

hook=features/data-volume/overlay/etc/boot2deb/first-boot.d/50-data-volume
[ -r "$hook" ] || { echo "run me from the boot2deb root" >&2; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
stubs="$work/stubs"
bin="$work/bin"
sys="$work/sys"
writes="$work/writes"
mounts="$work/mounts"
mkdir -p "$stubs" "$bin"

# The real tools the hook and the stubs use for text. Nothing that touches a disk.
for t in sh awk cat cut grep head ls readlink sed tr wc; do
    real="$(command -v "$t")" || { echo "the harness needs $t" >&2; exit 2; }
    ln -s "$real" "$bin/$t"
done

# Stubs for everything the hook shells out to. Each reads the scenario from STUB_*
# variables, so a case is a set of values rather than a fixture tree.
#
# STUB_DEVS lists the top-level block devices (what `lsblk -d` reports) and
# STUB_NODES further nodes a query can name directly, each as
# name=MAJ:MIN,TYPE,RO,TRAN. The sysfs tree under $sys is built from the same lists.
cat > "$stubs/findmnt" <<'S'
#!/bin/sh
[ "$STUB_ROOT_SRC" != fail ] || exit 1
echo "$STUB_ROOT_SRC"
S
cat > "$stubs/lsblk" <<'S'
#!/bin/sh
# STUB_LSBLK_FAIL lists the option clusters whose query fails, so a case can break
# one question without breaking the rest.
case " $STUB_LSBLK_FAIL " in *" $1 "*) exit 1 ;; esac
for last; do :; done
lookup() {
    for e in $STUB_DEVS $STUB_NODES; do
        case "$e" in "$1="*) printf '%s\n' "${e#*=}" | tr ',' ' '; return 0 ;; esac
    done
    return 1
}
case "$1" in
    -rnso)  for row in $STUB_ROOT_CHAIN; do printf '%s\n' "$row" | tr ',' ' '; done ;;
    -dno)   for e in $STUB_DEVS; do printf '%s %s\n' "${e%%=*}" "${e##*,}"; done ;;
    -dnro)  row="$(lookup "${last##*/}")" || { echo "lsblk: $last: not a block device" >&2; exit 32; }
            set -- $row; printf '%s %s %s\n' "$1" "$2" "$3" ;;
    -nro)   [ -z "$STUB_ROWS_EMPTY" ] || exit 0
            row="$(lookup "${last##*/}")" || exit 32
            set -- $row; printf '%s %s\n' "$1" "$STUB_MOUNTED"
            for c in $STUB_CHILDREN; do printf '%s \n' "$c"; done ;;
    -bdnro) printf '%s\n' "$STUB_SIZE" ;;
    -lno)   n="${last##*/}"; printf '%s\n%sp1\n' "$n" "$n" ;;
    *)      echo "lsblk stub: unmodeled query: $*" >&2; exit 99 ;;
esac
S
cat > "$stubs/blkid" <<'S'
#!/bin/sh
case "$1" in
    -L) if [ -n "$STUB_LABEL_DEV" ]; then echo "$STUB_LABEL_DEV"; exit 0; fi
        exit "$STUB_BLKID_L_RC" ;;
esac
echo "blkid stub: unmodeled query: $*" >&2
exit 99
S
cat > "$stubs/wipefs" <<'S'
#!/bin/sh
for a; do
    case "$a" in -a|--all|-o|--offset|--offset=*) echo "wipefs stub: refusing to model a wipe" >&2; exit 99 ;; esac
done
[ -z "$STUB_SIGS" ] || printf '%s\n' "$STUB_SIGS" | tr ',' '\n'
exit "$STUB_WIPEFS_RC"
S
cat > "$stubs/dd" <<'S'
#!/bin/sh
if [ -n "$STUB_UNREADABLE" ]; then echo "dd: error reading: Input/output error" >&2; exit 1; fi
bs=512 count=0
for a; do case "$a" in bs=*) bs="${a#bs=}" ;; count=*) count="${a#count=}" ;; esac; done
head -c "$((bs * count))" /dev/zero
S
printf '#!/bin/sh\necho "mount $*" >> "%s"\n' "$mounts" > "$stubs/mount"
# Nothing is mounted yet, so the hook always reaches `mount`, which records it.
printf '#!/bin/sh\nexit 1\n' > "$stubs/mountpoint"
for c in mkdir udevadm sleep; do
    printf '#!/bin/sh\nexit 0\n' > "$stubs/$c"
done
# The two that must never run except on a blank disk.
printf '#!/bin/sh\ncat > /dev/null\necho "sfdisk $*" >> "%s"\n' "$writes" > "$stubs/sfdisk"
printf '#!/bin/sh\necho "mkfs.ext4 $*" >> "%s"\n' "$writes" > "$stubs/mkfs.ext4"
chmod +x "$stubs"/*

# A copy of the hook reading our config and our sysfs instead of the real ones.
sed -e "s|^conf=/etc/boot2deb/data-volumes.conf$|conf=$work/conf|" \
    -e "s|^sys=/sys$|sys=$sys|" "$hook" > "$work/hook"
grep -q "^conf=$work/conf$" "$work/hook" && grep -q "^sys=$sys$" "$work/hook" \
    || { echo "the hook's conf= or sys= line moved; update the rewrite above" >&2; exit 2; }

# The sysfs nodes for every device in the scenario: a holders directory each, a
# `partition` attribute for partitions and for each MAJ:MIN in STUB_SYSFS_PART, and
# STUB_HOLDERS (MAJ:MIN=holder) entries.
build_sys() {
    rm -rf "$sys"
    mkdir -p "$sys/dev/block"
    for e in $STUB_DEVS $STUB_NODES; do
        set -- $(printf '%s' "${e#*=}" | tr ',' ' ')
        mkdir -p "$sys/dev/block/$1/holders"
        if [ "$2" = part ]; then : > "$sys/dev/block/$1/partition"; fi
    done
    for p in $STUB_SYSFS_PART; do : > "$sys/dev/block/$p/partition"; done
    for h in $STUB_HOLDERS; do : > "$sys/dev/block/${h%%=*}/holders/${h#*=}"; done
}

# A Turing RK1 as `lsblk` reports it: eMMC holding root, its two read-only boot
# hardware partitions (which lsblk lists as whole disks), and an NVMe.
rk1="mmcblk0=179:0,disk,0, mmcblk0boot0=179:8,disk,1, mmcblk0boot1=179:16,disk,1, nvme0n1=259:0,disk,0,nvme"
# The extra nodes: root's partition, a partition on the NVMe, and a LUKS mapping.
nodes="mmcblk0p2=179:2,part,0, nvme0n1p1=259:1,part,0, b2dcrypt=253:0,crypt,0,"

fails=0
# case_ <name> <expect: wrote|clean|adopted> <conf-line> [reason] [STUB_X=y ...]
#
# A fourth argument not starting with STUB_ is a fixed string the hook's log must
# contain.
case_() {
    name="$1"; expect="$2"; conf="$3"; shift 3
    reason=''
    case "${1:-}" in STUB_*|'') ;; *) reason="$1"; shift ;; esac
    : > "$writes"
    : > "$mounts"
    printf '%s\n' "$conf" > "$work/conf"
    rc=0
    (
        # The default world, then the case's changes to it. A subshell, so one
        # case's values never reach the next.
        export STUB_DEVS="$rk1" STUB_NODES="$nodes" STUB_ROOT_SRC=/dev/mmcblk0p2 \
            STUB_ROOT_CHAIN="179:2,part 179:0,disk" STUB_LSBLK_FAIL='' STUB_ROWS_EMPTY='' \
            STUB_MOUNTED='' STUB_CHILDREN='' STUB_HOLDERS='' STUB_SIZE=512110190592 \
            STUB_LABEL_DEV='' STUB_BLKID_L_RC=2 STUB_SIGS='' STUB_WIPEFS_RC=0 \
            STUB_UNREADABLE='' STUB_ABSENT='' STUB_SYSFS_PART=''
        for kv; do export "$kv"; done
        build_sys
        rm -rf "$work/path"
        mkdir "$work/path"
        for s in "$stubs"/*; do
            case " $STUB_ABSENT " in *" ${s##*/} "*) continue ;; esac
            ln -s "$s" "$work/path/${s##*/}"
        done
        PATH="$work/path:$bin" sh "$work/hook" > "$work/out" 2>&1
    ) || rc=$?
    ok=yes
    if [ "$rc" -ne 0 ]; then
        echo "FAIL  $name — the hook exited $rc:"; sed 's/^/        /' "$work/out"
        ok=''
    elif [ "$expect" != wrote ] && [ -s "$writes" ]; then
        echo "FAIL  $name — wrote to a disk it should not have:"; sed 's/^/        /' "$writes"
        ok=''
    elif [ "$expect" = wrote ] && [ ! -s "$writes" ]; then
        echo "FAIL  $name — did not create the volume:"; sed 's/^/        /' "$work/out"
        ok=''
    elif [ "$expect" = adopted ] && ! grep -qx 'mount /srv' "$mounts"; then
        echo "FAIL  $name — did not mount the volume it should adopt:"; sed 's/^/        /' "$work/out"
        ok=''
    elif [ -n "$reason" ] && ! grep -qF -- "$reason" "$work/out"; then
        echo "FAIL  $name — the log does not say \"$reason\":"; sed 's/^/        /' "$work/out"
        ok=''
    fi
    if [ -n "$ok" ]; then echo "ok    $name"; else fails=$((fails + 1)); fi
}

vol()      { printf '%s\t%s\text4\t/srv\t%s' "$1" "b2d-data" "${2:-if-blank}"; }
vol_nvme="$(vol nvme)"
vol_never="$(vol nvme never)"
vol_sata="$(vol sata)"
vol_usb="$(vol usb)"
vol_mmc="$(vol mmc)"

echo "-- the ladder"
case_ "creates on a genuinely blank disk" wrote "$vol_nvme"
case_ "adopts a volume that already has our label" adopted "$vol_nvme" \
    STUB_LABEL_DEV=/dev/nvme0n1p1
case_ "refuses a disk holding a partition table" clean "$vol_nvme" "gpt at 0x200" \
    STUB_SIGS="gpt 0x200,PMBR 0x1fe"
case_ "refuses a disk holding a bare filesystem" clean "$vol_nvme" "ext4 at 0x438" \
    STUB_SIGS="ext4 0x438"
case_ "refuses colliding signatures that blkid would call ambivalent" clean "$vol_nvme" \
    "vfat at 0x0, ext4 at 0x438" STUB_SIGS="vfat 0x0,ext4 0x438"
case_ "refuses a disk the kernel already sees partitions on" clean "$vol_nvme" \
    "sees partitions" STUB_CHILDREN=259:1
case_ "refuses an ambiguous match rather than guessing" clean "$vol_nvme" "matches 2 disks" \
    STUB_DEVS="$rk1 nvme1n1=259:2,disk,0,nvme"
case_ "refuses to create under create=never" clean "$vol_never" "create=never"
case_ "leaves the mount alone when no disk matches" clean "$vol_nvme" "no disk matches" \
    STUB_DEVS="mmcblk0=179:0,disk,0,"

echo "-- a probe that fails is never read as blank"
case_ "refuses when wipefs cannot probe the disk" clean "$vol_nvme" "wipefs cannot probe" \
    STUB_WIPEFS_RC=1
case_ "refuses a disk that does not read back" clean "$vol_nvme" "do not read back" \
    STUB_UNREADABLE=yes
case_ "refuses when lsblk cannot list the disks" clean "$vol_nvme" "cannot list the nvme disks" \
    STUB_LSBLK_FAIL=-dno
case_ "refuses when lsblk cannot identify the disk" clean "$vol_nvme" "cannot establish what" \
    STUB_LSBLK_FAIL=-dnro
case_ "refuses when lsblk cannot list what is on the disk" clean "$vol_nvme" \
    "lsblk cannot list it" STUB_LSBLK_FAIL=-nro
case_ "refuses when lsblk lists nothing for the disk" clean "$vol_nvme" \
    "lsblk lists nothing for it" STUB_ROWS_EMPTY=yes
case_ "refuses when lsblk cannot size the disk" clean "$vol_nvme" "size cannot be read" \
    STUB_LSBLK_FAIL=-bdnro
case_ "refuses when blkid cannot search for the label" clean "$vol_nvme" "could not search" \
    STUB_BLKID_L_RC=4
for tool in findmnt blkid lsblk wipefs dd sfdisk mkfs.ext4; do
    case_ "refuses when $tool is not installed" clean "$vol_nvme" "$tool" STUB_ABSENT="$tool"
done
case_ "refuses a disk something holds" clean "$vol_nvme" "held by dm-0" \
    STUB_HOLDERS=259:0=dm-0
case_ "refuses a disk that is mounted whole" clean "$vol_nvme" "a mount on it" \
    STUB_MOUNTED=/mnt
case_ "refuses a disk too small to read both ends of" clean "$vol_nvme" "smaller than 2 MiB" \
    STUB_SIZE=1048576

echo "-- root that cannot be established"
case_ "refuses when findmnt cannot name root's source" clean "$vol_nvme" \
    "cannot establish which disk holds root" STUB_ROOT_SRC=fail
case_ "refuses when root is not on a block device" clean "$vol_nvme" \
    "cannot establish which disk holds root" STUB_ROOT_SRC=overlay
case_ "refuses when root's chain cannot be walked" clean "$vol_nvme" \
    "cannot establish which disk holds root" STUB_LSBLK_FAIL=-rnso
case_ "refuses when root's chain reaches no disk" clean "$vol_nvme" \
    "cannot establish which disk holds root" STUB_ROOT_CHAIN="7:0,loop"
case_ "still adopts when root cannot be established" adopted "$vol_nvme" \
    STUB_ROOT_SRC=fail STUB_LABEL_DEV=/dev/nvme0n1p1

echo "-- picking the right disk"
# The case this exists for: sata and usb share the /dev/sd* name, so only the
# transport separates an internal disk from a drive somebody plugged in.
case_ "never writes a USB disk when the volume asks for SATA" clean "$vol_sata" \
    STUB_DEVS="$rk1 sda=8:0,disk,0,usb"
case_ "never writes a SATA disk when the volume asks for USB" clean "$vol_usb" \
    STUB_DEVS="$rk1 sda=8:0,disk,0,sata"
case_ "writes the USB disk when that is what was asked for" wrote "$vol_usb" \
    STUB_DEVS="$rk1 sda=8:0,disk,0,usb"
case_ "skips a /dev/sd* whose transport it cannot read" clean "$vol_sata" \
    STUB_DEVS="$rk1 sda=8:0,disk,0,"
case_ "never mistakes an NVMe volume for a USB disk" clean "$vol_nvme" \
    STUB_DEVS="mmcblk0=179:0,disk,0, sda=8:0,disk,0,usb"
case_ "on a real RK1, an nvme volume finds only the NVMe" clean "$vol_nvme" "gpt at 0x200" \
    STUB_SIGS="gpt 0x200"
# mmcblk<n>boot<n> are TYPE=disk and read-only; an mmc volume must not see them,
# and mmcblk0 itself is root's disk, so nothing is left to match.
case_ "never touches the eMMC boot hardware partitions" clean "$vol_mmc" "no disk matches"
case_ "never takes a read-only disk even if its name matched" clean "$vol_mmc" "no disk matches" \
    STUB_DEVS="mmcblk0=179:0,disk,0, mmcblk1=179:32,disk,1,"
case_ "takes the second mmc device when it is blank and writable" wrote "$vol_mmc" \
    STUB_DEVS="$rk1 mmcblk1=179:32,disk,0,"

echo "-- an explicit device"
case_ "writes an explicitly named blank disk" wrote "$(vol /dev/nvme0n1)"
case_ "refuses an explicitly named partition" clean "$(vol /dev/nvme0n1p1)" \
    "not a writable whole disk"
case_ "refuses an explicitly named read-only disk" clean "$(vol /dev/mmcblk0boot0)" \
    "not a writable whole disk"
case_ "refuses an explicitly named device-mapper node" clean "$(vol /dev/mapper/b2dcrypt)" \
    "not a writable whole disk"
case_ "refuses an explicitly named root disk" clean "$(vol /dev/mmcblk0)" "is under root"
# The guard compares device numbers along root's whole chain, so a root on a LUKS
# or LVM layer still excludes the disk at the bottom of it.
case_ "refuses the root disk reached through a device-mapper layer" clean "$(vol /dev/mmcblk0)" \
    "is under root" STUB_ROOT_SRC=/dev/mapper/b2dcrypt \
    STUB_ROOT_CHAIN="253:0,crypt 179:2,part 179:0,disk"
case_ "refuses a node a partition attribute marks, whatever lsblk says" clean "$(vol /dev/nvme0n1)" \
    "not a writable whole disk" STUB_SYSFS_PART=259:0
case_ "refuses a device lsblk does not know" clean "$(vol /dev/nvme9n9)" "cannot establish what"

if [ "$fails" -eq 0 ]; then
    echo "all cases passed"
else
    echo "$fails case(s) failed" >&2
    exit 1
fi
