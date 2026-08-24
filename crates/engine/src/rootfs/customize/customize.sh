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
# POSIX sh, no bashisms: the cage runs the target's `/bin/sh`, which on a Debian base
# is dash.
#
# Environment, all set by the caller:
#
#   B2D_USER                    the default account's name
#   B2D_SUDOERS                 its sudoers spec, e.g. `NOPASSWD: ALL`
#   B2D_AUTHORIZED_KEYS         its authorized_keys, one per line; empty for none
#   B2D_LOCAL_REPO              the build-time local apt source's stem
#   B2D_INITRAMFS_STUB          the update-initramfs placeholder to remove
#   B2D_INITRAMFS_STUB_LOG      where the placeholder recorded its calls
#   B2D_TIMEZONE                the resolved tzdata zone name
#   B2D_LOCALES_GENERATED       non-empty when locale-gen was asked for anything
#   B2D_DEPTHCHARGE_BOARD       the board profile, or empty on a raw-gap board
#   B2D_DEPTHCHARGE_CONFIG      the armed /etc/depthcharge-tools/config content
#   B2D_REQUIRED_INITRD_MODULES space-separated modules the signed initramfs must hold
set -eu

# --- The default account -----------------------------------------------------
#
# Created *locked*: the unique per-image first-boot password is spliced into
# /etc/shadow at image assembly, so the provisioned tree stays cacheable across images
# built from one build point.
useradd -m -s /bin/bash "$B2D_USER"
usermod -aG video,render "$B2D_USER"

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
# `chown "$B2D_USER":` — a trailing colon with no group names the account's *login*
# group, whatever it is called. useradd derives that name from the target's login.defs,
# which is the target's policy and not something to restate here.
if [ -n "$B2D_AUTHORIZED_KEYS" ]; then
    install -d -m 0700 "/home/$B2D_USER/.ssh"
    printf '%s\n' "$B2D_AUTHORIZED_KEYS" > "/home/$B2D_USER/.ssh/authorized_keys"
    chmod 0600 "/home/$B2D_USER/.ssh/authorized_keys"
    chown -R "$B2D_USER": "/home/$B2D_USER/.ssh"
fi

# Regenerated on first boot, so every image does not ship one identity.
rm -f /etc/ssh/ssh_host_*

# The build-time-only local `.deb` repository: its `file://` temp dir is gone by the
# time the image runs, so leaving the source would fail every on-device `apt-get
# update`. The feature repositories stay — those are meant to persist.
rm -f "/etc/apt/sources.list.d/$B2D_LOCAL_REPO.list"

# --- Boot artifacts ----------------------------------------------------------
#
# Remove the update-initramfs placeholder and say what it absorbed, then re-run the
# kernel postinst.d hooks. This is the first moment an initramfs can be built
# correctly: the overlay is in place and the depmod hook — which run-parts reaches
# first — has written the modules.dep that resolves this board's out-of-tree modules.
#
# The count comes from the placeholder's own log rather than from an assumption about
# what dpkg would have done, and its absence is reported rather than passed over: the
# placeholder works by shadowing the real tool on PATH, so "never called" is a real
# outcome and the build should say so instead of implying a saving it did not make.
rm -f "$B2D_INITRAMFS_STUB"
if [ -f "$B2D_INITRAMFS_STUB_LOG" ]; then
    echo "suppressed $(wc -l < "$B2D_INITRAMFS_STUB_LOG") initramfs build(s) during the package install; building the real one now"
    rm -f "$B2D_INITRAMFS_STUB_LOG"
else
    echo 'note: the update-initramfs placeholder was never called, so the package install built its own initrd' >&2
fi

# --exit-on-error fails the build rather than shipping a kernel with nothing to boot
# it. The version is reused by the depthcharge tail below.
kver="$(linux-version list | linux-version sort --reverse | head -n1)"
run-parts --exit-on-error --arg="$kver" /etc/kernel/postinst.d

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
# Enable systemd-time-wait-sync, so time-sync.target means what Debian's maintenance
# jobs already assume it means. apt-daily, logrotate, man-db, fstrim, e2scrub_all,
# dpkg-db-backup and anacron all order themselves After=time-sync.target, and nothing
# reaches that target unless this unit is enabled — so on a stock image the ordering is
# inert and they run against whatever the clock says at boot, which on a board with no
# RTC is the mtime of /var/lib/systemd/timesync/clock.
#
# Enabled here rather than by a .wants symlink in the base overlay because the unit
# ships inside the systemd package: a symlink laid down before that package installs is
# one deb-systemd-helper may still have an opinion about when it applies the unit's
# preset; one written afterwards is the final word. The link is the exact one
# `systemctl enable` would create, written directly because systemctl in a cage with no
# running manager is a larger dependency than one `ln -s`.
#
# Both asserts guard the base overlay's bounded-wait drop-in: it is inert if the unit
# is missing, and it fails closed — holding the boot for the full 45 seconds on every
# offline boot — if `timeout` is.
[ -f /usr/lib/systemd/system/systemd-time-wait-sync.service ] ||
    { echo 'systemd ships no systemd-time-wait-sync.service: the bounded-wait drop-in would configure nothing' >&2; exit 1; }
[ -x /usr/bin/timeout ] ||
    { echo 'coreutils ships no /usr/bin/timeout: the bounded-wait drop-in would never release the boot' >&2; exit 1; }
mkdir -p /etc/systemd/system/sysinit.target.wants
ln -sf /usr/lib/systemd/system/systemd-time-wait-sync.service \
    /etc/systemd/system/sysinit.target.wants/systemd-time-wait-sync.service

# --- Depthcharge -------------------------------------------------------------
#
# Build the signed kernel partition, prove it is bootable, and arm the on-device kernel
# hooks. Every check here guards a failure that is silent on serial-console-less
# hardware. Skipped entirely on a raw-gap board, which has no board profile.
[ -n "$B2D_DEPTHCHARGE_BOARD" ] || exit 0

# Assert every module the initramfs lists actually exists for this kernel: MODULES=list
# silently drops an unresolvable name, so a typo would ship an initramfs missing (say)
# the PMIC driver and the board would hang at a white screen. `</dev/null` so the inner
# command cannot consume the list.
for list in /usr/share/initramfs-tools/modules.d/*; do
    [ -f "$list" ] || continue
    while read -r mod; do
        case "$mod" in ''|\#*) continue ;; esac
        modprobe --set-version "$kver" --show-depends "$mod" </dev/null >/dev/null 2>&1 || {
            echo "initramfs module '$mod' does not exist in kernel $kver (from $(basename "$list"))" >&2
            exit 1
        }
    done < "$list"
done

# Build the signed payload; board profile and cmdline come from the pre-install
# overlay's config, root= from /etc/fstab.
depthchargectl build --verbose
kpart="$(ls /boot/depthcharge/*.img 2>/dev/null | head -n1)"
[ -n "$kpart" ] || { echo 'depthchargectl build produced no image' >&2; exit 1; }
futility vbutil_kernel --verify "/boot/depthcharge/$(basename "$kpart")"

# The initramfs is inside the signature now — last chance to confirm the modules that
# must be in it actually are.
initrd_list="$(lsinitramfs "/boot/initrd.img-$kver")"
for need in $B2D_REQUIRED_INITRD_MODULES; do
    case "$initrd_list" in *"$need"*) ;; *)
        echo "the built initramfs is missing $need — MODULES=list did not take" >&2
        exit 1 ;;
    esac
done

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
