# The target-side initrd report, run in a cage over the customized rootfs, where the
# rootfs is `/`. It prints what the built initrd holds, and the build decides whether
# that covers the image's initramfs module list (`core::initramfs::coverage`).
#
# A *constant*, like customize.sh: one committed file, byte-identical for every build.
# It takes no input at all. It reports and never judges, so the rule it feeds lives in
# Rust, where it is unit-tested.
#
# POSIX sh, no bashisms: the cage runs the target's `/bin/sh`, which on a Debian base
# is dash. Each line is one fact, tagged by its first word, in the grammar
# `core::initramfs::InitrdReport` documents:
#
#   kernel <release>                 the kernel the initrd was built for
#   initrd <path>                    one path the initrd holds
#   builtin <path>                   one line of that kernel's modules.builtin
#   firmware <module path> <file>    one firmware file a module in the initrd declares
set -eu

# The same kernel customize.sh built the initrd for: the newest installed release.
kver="$(linux-version list | linux-version sort --reverse | head -n1)"
initrd="/boot/initrd.img-$kver"
[ -e "$initrd" ] || { echo "no initrd at $initrd to report on" >&2; exit 1; }
printf 'kernel %s\n' "$kver"

# The listing is kept in the cage's /tmp for the firmware pass below, rather than
# listed twice: lsinitramfs decompresses the whole archive each time it runs.
listing=/tmp/boot2deb-initrd-listing
lsinitramfs "$initrd" > "$listing"
sed 's/^/initrd /' "$listing"

# modules.builtin sits under /usr/lib on a merged-/usr image and /lib otherwise.
# A kernel with no built-in modules ships none, which reports no builtin lines.
for builtin in "/usr/lib/modules/$kver/modules.builtin" "/lib/modules/$kver/modules.builtin"; do
    if [ -r "$builtin" ]; then
        sed 's/^/builtin /' "$builtin"
        break
    fi
done

# The firmware every module in the initrd declares, read from the same module file
# in the rootfs: the initrd's copy is that file, so modinfo needs no extraction.
grep -E '(^|/)lib/modules/[^/]+/.+\.ko(\.(xz|zst|gz))?$' "$listing" | while read -r module; do
    modinfo -F firmware "/$module" 2>/dev/null | sed "s|^|firmware $module |"
done
rm -f "$listing"
