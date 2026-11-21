# The target-side customize program, run once in a cage where the rootfs is `/`.
#
# It is a *constant*: one committed file, byte-identical for every build, and every
# value it acts on arrives in the environment. Nothing this build resolved is ever
# interpolated into shell syntax, so a hostname carrying a newline or a board profile
# spelling the heredoc delimiter cannot change what runs — it can only be a wrong
# value in a right program. The Rust side (`rootfs::provisioner::customize`) resolves
# and validates; this reads and acts, which is the same split the on-image selftest
# runner and `core::expect` use.
#
# POSIX sh, no bashisms: the cage runs the target's `/bin/sh`, which on Debian and
# Ubuntu is dash. The coreutils it calls may be GNU's or uutils', so it uses only the
# forms the two agree on.
#
# Environment, all set by the caller:
#
#   B2D_USER                    the default account's name
#   B2D_APT_REPLACED            the bootstrap's own apt sources files, one path per line
#   B2D_SUDOERS                 its sudoers spec, e.g. `NOPASSWD: ALL`
#   B2D_AUTHORIZED_KEYS         its authorized_keys, one per line; empty for none
#   B2D_GROUPS                  its supplementary groups, comma-separated; empty for none
#   B2D_INITRAMFS_STUBS         the initramfs placeholders to remove, one path per line
#   B2D_INITRAMFS_STUB_LOG      where the placeholders recorded their calls
#   B2D_TIMEZONE                the resolved tzdata zone name
#   B2D_CLOCK_WAIT_UNIT         the unit that holds time-sync.target until the first sync
#   B2D_CLOCK_WAIT_WANTED_BY    the target that unit's [Install] section names
#   B2D_CLOCK_WAIT_PROGRAM      the program that unit's bounded drop-in runs
#   B2D_LOCALES_GENERATED       non-empty when locale-gen was asked for anything
#   B2D_DEPTHCHARGE_BOARD       the board profile, or empty on a raw-gap board
#   B2D_DEPTHCHARGE_CONFIG      the armed /etc/depthcharge-tools/config content
#   B2D_GRUB_PLATFORM           GRUB's platform (arm64-efi) under the efi loader, else empty
#   B2D_GRUB_PACKAGE            the package that installs GRUB for that platform
#   B2D_GRUB_REMOVABLE          the removable-media loader's file name (BOOTAA64.EFI)
#   B2D_GRUB_FILE               the loader's file name under the distributor's directory
#   B2D_GRUB_FS_MODULE          the GRUB module that reads the rootfs filesystem
#   B2D_ESP_MOUNT               where the EFI system partition is mounted on the board
#   B2D_ROOTFS_PARTITION        the rootfs partition's GPT entry number
#   B2D_ROOTFS_PARTUUID         the rootfs partition's PARTUUID
#   B2D_ROOTFS_FS_UUID          the rootfs filesystem's UUID
#
# The initramfs this builds is checked against the image's initramfs module list
# afterwards, by initrd-report.sh beside this file and the build's own rule.
set -eu

# --- The default account -----------------------------------------------------
#
# Created *locked*: the unique per-image first-boot password is spliced into
# /etc/shadow at image assembly, so the provisioned tree stays cacheable across images
# built from one build point.
useradd -m -s /bin/bash "$B2D_USER"

# Supplementary groups: the account's standing hardware access, resolved from config.
# `usermod` fails on a group the target does not have, and under `set -e` that fails the
# build — which is what should happen, since a group no installed package created is a
# name nobody will ever be a useful member of.
[ -z "$B2D_GROUPS" ] || usermod -aG "$B2D_GROUPS" "$B2D_USER"

# The sudoers drop-in. Written here rather than staged as an overlay file because the
# mode matters and the overlay staging pass normalizes every mode it copies: sudo
# refuses a drop-in that is not 0440.
mkdir -p /etc/sudoers.d
printf '%s ALL=(ALL) %s\n' "$B2D_USER" "$B2D_SUDOERS" > "/etc/sudoers.d/$B2D_USER"
chmod 0440 "/etc/sudoers.d/$B2D_USER"

# The authorized keys, for the same reason: sshd under its default StrictModes refuses
# a key whose file or containing directories are group- or world-writable, and a staged
# `.ssh` would arrive at 0755. Written after `useradd -m` has made the home directory.
#
# The group is the account's *login* group, whatever it is called: useradd derives that
# name from the target's login.defs, which is the target's policy and not something to
# restate here, so `id -gn` reads it back. GNU chown takes a trailing colon to mean the
# same, but uutils' chown leaves the group unchanged.
if [ -n "$B2D_AUTHORIZED_KEYS" ]; then
    install -d -m 0700 "/home/$B2D_USER/.ssh"
    printf '%s\n' "$B2D_AUTHORIZED_KEYS" > "/home/$B2D_USER/.ssh/authorized_keys"
    chmod 0600 "/home/$B2D_USER/.ssh/authorized_keys"
    chown -R "$B2D_USER:$(id -gn "$B2D_USER")" "/home/$B2D_USER/.ssh"
fi

# Regenerated on first boot, so every image does not ship one identity.
rm -f /etc/ssh/ssh_host_*

# The same for the machine ID. The image ships /etc/machine-id empty, and systemd
# fills an empty one at first boot from /var/lib/dbus/machine-id whenever that holds
# an ID, which anything running `dbus-uuidgen --ensure` writes. Every board flashed
# from this image would then boot with the same machine ID. The link makes the two
# files one, as machine-id(5) describes, so systemd generates a fresh ID and D-Bus
# reads it too.
if [ -d /var/lib/dbus ]; then
    ln -sf /etc/machine-id /var/lib/dbus/machine-id
fi

# The bootstrap's own apt configuration for the distribution's archive: the release
# pocket's sources.list, each further pocket's sources.list.d entry, and the keyring
# copies they name. The image's deb822 sources under sources.list.d replace all of it
# and trust the keyring the distribution's keyring package installs, whose upgrades
# follow the archive's key rotation. Left in place, the copies are a second keyring no
# rotation ever reaches and a second set of sources apt reads twice.
printf '%s\n' "$B2D_APT_REPLACED" | while IFS= read -r replaced; do
    [ -z "$replaced" ] || rm -f -- "$replaced"
done
rm -f /usr/share/keyrings/fcage-archive-keyring.gpg

# --- Boot artifacts ----------------------------------------------------------
#
# Remove the initramfs generator's placeholders and say what they absorbed, then
# re-run the kernel postinst.d hooks. This is the first moment an initramfs can be
# built correctly: the overlay is in place and the depmod hook — which run-parts
# reaches first — has written the modules.dep that resolves this board's out-of-tree
# modules.
#
# The count comes from the placeholders' own log rather than from an assumption about
# what dpkg would have done, and its absence is reported rather than passed over: a
# placeholder works by shadowing the real tool on PATH, so "never called" is a real
# outcome and the build should say so instead of implying a saving it did not make.
printf '%s\n' "$B2D_INITRAMFS_STUBS" | while IFS= read -r stub; do
    [ -z "$stub" ] || rm -f -- "$stub"
done
if [ -f "$B2D_INITRAMFS_STUB_LOG" ]; then
    echo "suppressed $(wc -l < "$B2D_INITRAMFS_STUB_LOG") initramfs build(s) during the package install; building the real one now"
    rm -f "$B2D_INITRAMFS_STUB_LOG"
else
    echo 'note: no initramfs placeholder was called, so the package install built its own initrd' >&2
fi

# --exit-on-error fails the build rather than shipping a kernel with nothing to boot
# it. Under the efi loader the EFI system partition does not exist yet: its hooks write
# into the directory the image node formats it from, and BOOT2DEB_ESP_STAGED says so.
kver="$(linux-version list | linux-version sort --reverse | head -n1)"
if [ -n "$B2D_GRUB_PLATFORM" ]; then
    mkdir -p "$B2D_ESP_MOUNT"
    BOOT2DEB_ESP_STAGED=1 run-parts --exit-on-error --arg="$kver" /etc/kernel/postinst.d
else
    run-parts --exit-on-error --arg="$kver" /etc/kernel/postinst.d
fi

# --- The EFI loader ----------------------------------------------------------
#
# Install GRUB as `grub-install --force-extra-removable --no-nvram` would. That command
# probes the block devices under /boot and the EFI system partition, and this cage has
# none: the partition does not exist until the image node formats it from what this
# step writes under $B2D_ESP_MOUNT. So each of its steps is taken here directly, from
# the same package. On the board, the GRUB package's postinst runs grub-install itself
# on every upgrade, against the real devices, because the core image below exists.
# Skipped under any other loader.
if [ -n "$B2D_GRUB_PLATFORM" ]; then
    grub_lib="/usr/lib/grub/$B2D_GRUB_PLATFORM"
    grub_dir="/boot/grub/$B2D_GRUB_PLATFORM"

    # The modules and lists the core image loads from /boot/grub at boot, from the same
    # package as the core image, then the font and the translations: what grub-install
    # copies.
    rm -rf "$grub_dir"
    mkdir -p "$grub_dir" /boot/grub/fonts /boot/grub/locale
    for f in "$grub_lib"/*; do
        [ ! -f "$f" ] || cp "$f" "$grub_dir/"
    done
    [ ! -f /usr/share/grub/unicode.pf2 ] || cp /usr/share/grub/unicode.pf2 /boot/grub/fonts/
    for mo in /usr/share/locale/*/LC_MESSAGES/grub.mo; do
        [ -f "$mo" ] || continue
        lang=${mo#/usr/share/locale/}
        cp "$mo" "/boot/grub/locale/${lang%%/*}.mo"
    done
    # The environment block, as grub-install writes it: the signature, then `#` to 1024
    # bytes. Not through grub-editenv, which probes the device under the file first,
    # and this cage's is the build host's.
    if [ ! -f /boot/grub/grubenv ]; then
        { printf '# GRUB Environment Block\n'; head -c 999 /dev/zero | tr '\0' '#'; } \
            > /boot/grub/grubenv
    fi

    # The core image. Its embedded configuration finds /boot/grub on the rootfs
    # partition of the disk the firmware loaded it from. GRUB starts with $root set to
    # the partition it was loaded from (hd0,gpt2), so the disk is $root up to its comma.
    # It therefore names no identifier, and on a board with several disks it boots the
    # rootfs beside its own partition. GRUB runs it before any module is read from disk,
    # so it is plain commands, and the modules it needs are built in.
    early=$(mktemp)
    {
        printf '%s\n' "regexp -s 1:b2d_disk '^([^,]+),' \"\$root\""
        printf 'set root="$b2d_disk,gpt%s"\n' "$B2D_ROOTFS_PARTITION"
        printf '%s\n' 'set prefix="($root)/boot/grub"'
    } > "$early"
    grub-mkimage -O "$B2D_GRUB_PLATFORM" -d "$grub_lib" -p /boot/grub -c "$early" \
        -o "$grub_dir/core.efi" part_gpt "$B2D_GRUB_FS_MODULE" regexp
    rm -f "$early"

    # The two copies grub-install --force-extra-removable leaves: under the distributor's
    # directory, named as the GRUB package's postinst names it (the first word of
    # GRUB_DISTRIBUTOR, lowercased), and at the removable-media path the firmware loads.
    distributor=$(
        set +u
        for x in /etc/default/grub /etc/default/grub.d/*.cfg; do
            [ ! -e "$x" ] || . "$x"
        done
        printf '%s' "$GRUB_DISTRIBUTOR" | tr 'A-Z' 'a-z' | cut -d' ' -f1
    )
    [ -n "$distributor" ] ||
        { echo 'GRUB_DISTRIBUTOR is empty: no directory to install GRUB under' >&2; exit 1; }
    mkdir -p "$B2D_ESP_MOUNT/EFI/BOOT" "$B2D_ESP_MOUNT/EFI/$distributor"
    cp "$grub_dir/core.efi" "$B2D_ESP_MOUNT/EFI/$distributor/$B2D_GRUB_FILE"
    cp "$grub_dir/core.efi" "$B2D_ESP_MOUNT/EFI/BOOT/$B2D_GRUB_REMOVABLE"

    # What the GRUB package's postinst asks debconf on every upgrade on the board:
    # reinstall into the removable path as well as the distributor's, and write no
    # firmware variable. None of these boards keeps one a running Linux writes.
    printf '%s\n' \
        "$B2D_GRUB_PACKAGE grub2/force_efi_extra_removable boolean true" \
        "$B2D_GRUB_PACKAGE grub2/update_nvram boolean false" | debconf-set-selections

    # The menu, from Debian's own update-grub, so the image ships the file the kernel
    # hook regenerates on the board. grub-mkconfig asks grub-probe about the devices
    # under / and /boot, asks grub-mkrelpath where /boot sits in its filesystem, and
    # its 10_linux checks that /dev/disk/by-partuuid names the root before it writes
    # root=PARTUUID=. In this cage the answers would describe the build host: / is a
    # directory bind-mounted from the host's disk. For this one call, stand-ins answer
    # with what the board's own tools will find. grub-probe answers for the rootfs
    # partition the image node writes (its PARTUUID and filesystem UUID, ext4 on GPT,
    # no BIOS drive hint: an arm64 board's grub-probe has none either), grub-mkrelpath
    # for a root that is its filesystem's root, and the by-partuuid name exists. The
    # menu is then the one update-grub writes on the board. An unknown question fails the call rather than being guessed at. All are
    # gone again before the tree is exported.
    probe=/usr/sbin/grub-probe
    relpath=/usr/bin/grub-mkrelpath
    mv "$probe" "$probe.boot2deb-real"
    mv "$relpath" "$relpath.boot2deb-real"
    cat > "$relpath" <<'RELPATH'
#!/bin/sh
# boot2deb image build: grub-mkrelpath for one update-grub run. The image's root is
# its filesystem's root, so a path's place in that filesystem is the path itself.
[ $# -eq 1 ] || { echo "grub-mkrelpath stand-in: expected one path, got: $*" >&2; exit 1; }
readlink -f -- "$1"
RELPATH
    cat > "$probe" <<'PROBE'
#!/bin/sh
# boot2deb image build: grub-probe for one update-grub run, answering for the rootfs
# partition the image node writes.
target=
while [ $# -gt 0 ]; do
    case "$1" in
        -t|--target) target=$2; shift ;;
        --target=*) target=${1#--target=} ;;
        -d|--device) shift ;;
    esac
    shift
done
case "$target" in
    device) echo "/dev/disk/by-partuuid/$B2D_ROOTFS_PARTUUID" ;;
    fs) echo "$B2D_GRUB_FS_MODULE" ;;
    fs_uuid) echo "$B2D_ROOTFS_FS_UUID" ;;
    partuuid) echo "$B2D_ROOTFS_PARTUUID" ;;
    partmap) echo gpt ;;
    drive) echo "(hd0,gpt$B2D_ROOTFS_PARTITION)" ;;
    abstraction|compatibility_hint|cryptodisk_uuid|hints_string) ;;
    *) echo "grub-probe stand-in: no answer for --target=$target" >&2; exit 1 ;;
esac
PROBE
    chmod 0755 "$probe" "$relpath"
    mkdir -p /dev/disk/by-partuuid
    : > "/dev/disk/by-partuuid/$B2D_ROOTFS_PARTUUID"
    update-grub
    rm -f "/dev/disk/by-partuuid/$B2D_ROOTFS_PARTUUID"
    rmdir /dev/disk/by-partuuid /dev/disk 2>/dev/null || true
    mv -f "$probe.boot2deb-real" "$probe"
    mv -f "$relpath.boot2deb-real" "$relpath"
    grep -q "root=PARTUUID=$B2D_ROOTFS_PARTUUID " /boot/grub/grub.cfg ||
        { echo "update-grub wrote no entry rooting on PARTUUID=$B2D_ROOTFS_PARTUUID" >&2; exit 1; }
    grep -q "^[[:space:]]*linux[[:space:]]\{1,\}/boot/vmlinuz-$kver " /boot/grub/grub.cfg ||
        { echo "update-grub wrote no entry loading /boot/vmlinuz-$kver" >&2; exit 1; }
fi

# --- Localization ------------------------------------------------------------
#
# Prove the two things resolution could not. A timezone missing from the target's
# tzdata leaves /etc/localtime dangling and the clock silently wrong; a `locales`
# package that generated nothing leaves LANG naming an ungenerated locale.
[ -e "/usr/share/zoneinfo/$B2D_TIMEZONE" ] ||
    { echo "timezone '$B2D_TIMEZONE' is not in this suite's tzdata" >&2; exit 1; }
# The locale-archive is absent on an image that generates nothing (also what a base
# system looks like), so the check only means something when locales were asked for.
if [ -n "$B2D_LOCALES_GENERATED" ]; then
    [ -s /usr/lib/locale/locale-archive ] ||
        { echo 'locale-gen produced no locale-archive: LANG would name an ungenerated locale' >&2; exit 1; }
fi

# --- The bounded clock wait --------------------------------------------------
#
# Enable the time client's wait unit, so time-sync.target means what the distribution's
# maintenance jobs already assume it means. apt-daily, logrotate, man-db, fstrim,
# e2scrub_all and dpkg-db-backup all order themselves After=time-sync.target, and
# nothing reaches that target unless such a unit is enabled — so on a stock image the
# ordering is inert and they run against whatever the clock says at boot, which on a
# board with no RTC is stale.
#
# Enabled here rather than by a .wants symlink in an overlay because the unit ships
# inside the time client's package: a symlink laid down before that package installs is
# one deb-systemd-helper may still have an opinion about when it applies the unit's
# preset; one written afterwards is the final word. The link is the exact one
# `systemctl enable` would create, written directly because systemctl in a cage with no
# running manager is a larger dependency than one `ln -s`.
#
# Both asserts guard the overlay's bounded-wait drop-in: it is inert if the unit is
# missing, and it replaces the unit's command with one that cannot run if its program
# is.
unit="/usr/lib/systemd/system/$B2D_CLOCK_WAIT_UNIT"
[ -f "$unit" ] ||
    { echo "no $B2D_CLOCK_WAIT_UNIT is installed: the bounded-wait drop-in would configure nothing" >&2; exit 1; }
[ -x "$B2D_CLOCK_WAIT_PROGRAM" ] ||
    { echo "no $B2D_CLOCK_WAIT_PROGRAM is installed: the bounded-wait drop-in could not run" >&2; exit 1; }
mkdir -p "/etc/systemd/system/$B2D_CLOCK_WAIT_WANTED_BY.wants"
ln -sf "$unit" "/etc/systemd/system/$B2D_CLOCK_WAIT_WANTED_BY.wants/$B2D_CLOCK_WAIT_UNIT"

# --- Depthcharge -------------------------------------------------------------
#
# Build the signed kernel partition, prove it is bootable, and arm the on-device kernel
# hooks. Every check here guards a failure that is silent on serial-console-less
# hardware. Skipped entirely on a raw-gap board, which has no board profile.
[ -n "$B2D_DEPTHCHARGE_BOARD" ] || exit 0

# Build the signed payload; board profile and cmdline come from the pre-install
# overlay's config, root= from /etc/fstab.
depthchargectl build --verbose
kpart="$(ls /boot/depthcharge/*.img 2>/dev/null | head -n1)"
[ -n "$kpart" ] || { echo 'depthchargectl build produced no image' >&2; exit 1; }
futility vbutil_kernel --verify "/boot/depthcharge/$(basename "$kpart")"

# Arm the package's kernel hooks for the shipped system: an on-device apt kernel
# upgrade re-signs and writes the other slot itself. They were off during the build so
# they could not hunt the build host's disks.
printf '%s' "$B2D_DEPTHCHARGE_CONFIG" > /etc/depthcharge-tools/config
grep -q '^enable-system-hooks = True$' /etc/depthcharge-tools/config

# And assert the other half of the upgrade protocol is armed: the depthcharge-tools
# .service blesses a freshly-written slot once it boots. Without it, a successful
# kernel upgrade is rolled back one reboot later.
systemctl is-enabled depthcharge-tools.service >/dev/null || {
    echo 'depthcharge-tools.service is not enabled: a kernel upgrade would be' >&2
    echo 'rolled back one reboot after it succeeded (nothing would bless it)' >&2
    exit 1
}
