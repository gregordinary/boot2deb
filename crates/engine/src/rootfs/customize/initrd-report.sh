# The target-side initrd report, run in a cage over the customized rootfs, where the
# rootfs is `/`. It prints what the built initrd holds, and the build decides whether
# that covers the image's initramfs module list (`core::initramfs::coverage`) and the
# firmware of the kernel's built-in drivers
# (`core::initramfs::missing_builtin_firmware`).
#
# A *constant*, like customize.sh: one committed file, byte-identical for every build.
# Its inputs arrive in the environment:
#
#   B2D_INITRAMFS_GENERATOR       the generator that built the initrd (`initramfs-tools`
#                                 or `dracut`), which decides the lister
#   B2D_INITRD_MODULE_FIRMWARE    `yes` to report the firmware each module in the
#                                 initrd declares, which only an image with an
#                                 initramfs module list reads
#
# It reports and never judges, so the rules it feeds live in Rust, where they are
# unit-tested.
#
# POSIX sh, no bashisms: the cage runs the target's `/bin/sh`, which on a Debian base
# is dash. Each line is one fact, tagged by its first word, in the grammar
# `core::initramfs::InitrdReport` documents:
#
#   kernel <release>                    the kernel the initrd was built for
#   initrd <path>                       one path the initrd holds
#   builtin <path>                      one line of that kernel's modules.builtin
#   firmware <module path> <file>       one firmware file a module in the initrd declares
#   builtin-firmware <module> <file>    one firmware file a built-in driver declares
#   image-firmware <file>               a file a built-in driver declares that the
#                                       image holds, as the driver names it
set -eu

# The same kernel customize.sh built the initrd for: the newest installed release.
kver="$(linux-version list | linux-version sort --reverse | head -n1)"
initrd="/boot/initrd.img-$kver"
[ -e "$initrd" ] || { echo "no initrd at $initrd to report on" >&2; exit 1; }
printf 'kernel %s\n' "$kver"

# The listing is kept in the cage's /tmp for the firmware pass below, rather than
# listed twice: either lister decompresses the whole archive each time it runs.
#
# lsinitramfs prints one path per line. lsinitrd prints a header and then each
# archive's files as a cpio `ls -l` listing, where the path is everything after the
# eighth column and a link's target follows ` -> `. Only regular files, directories
# and links are kept: a device node's size column is a `major, minor` pair, which
# shifts the path, and no module or firmware file is one.
listing=/tmp/boot2deb-initrd-listing
case "$B2D_INITRAMFS_GENERATOR" in
    initramfs-tools)
        lsinitramfs "$initrd" > "$listing"
        ;;
    dracut)
        # Through a file rather than a pipe, so a failing lsinitrd fails the report.
        lsinitrd "$initrd" > "$listing.ls"
        sed -n -E '/^[-dl][-rwxsStT]{9} /{s/^([^ ]+ +){8}//; s/ -> .*$//; p;}' \
            "$listing.ls" > "$listing"
        rm -f "$listing.ls"
        ;;
    *)
        echo "no lister for the initramfs generator '$B2D_INITRAMFS_GENERATOR'" >&2
        exit 1
        ;;
esac
sed 's/^/initrd /' "$listing"

# modules.builtin sits under /usr/lib on a merged-/usr image and /lib otherwise.
# A kernel with no built-in modules ships none, which reports no builtin lines.
for builtin in "/usr/lib/modules/$kver/modules.builtin" "/lib/modules/$kver/modules.builtin"; do
    if [ -r "$builtin" ]; then
        sed 's/^/builtin /' "$builtin"
        break
    fi
done

# The firmware the kernel's built-in drivers declare. modules.builtin.modinfo holds one
# NUL-terminated `<module>.<key>=<value>` entry per field, and a module name has no
# dot, so the key starts after the first one. xargs -0 splits the entries, which keeps
# the split independent of how a coreutils `tr` treats a NUL.
#
# Each declared file the image holds is reported as well. It is looked up where the
# kernel's loader and both generators look: updates/<release>, updates, <release>,
# then the firmware root, each bare or compressed. A declaration with a `*` names no
# file, and neither generator expands one.
for modinfo in "/usr/lib/modules/$kver/modules.builtin.modinfo" "/lib/modules/$kver/modules.builtin.modinfo"; do
    if [ -r "$modinfo" ]; then
        xargs -0 -r printf '%s\n' < "$modinfo" \
            | sed -n 's/^\([^.]*\)\.firmware=\(.*\)$/\1 \2/p' \
            | while read -r module file; do
                printf 'builtin-firmware %s %s\n' "$module" "$file"
                for dir in "updates/$kver" updates "$kver" ''; do
                    for ext in '' .xz .zst; do
                        if [ -e "/lib/firmware/${dir:+$dir/}$file$ext" ]; then
                            printf 'image-firmware %s\n' "$file"
                            continue 3
                        fi
                    done
                done
            done
        break
    fi
done

# The firmware every module in the initrd declares, read from the same module file
# in the rootfs: the initrd's copy is that file, so modinfo needs no extraction. An
# initrd built without a module list holds every module the generator chose, and
# reading each under emulation would cost minutes for warnings nobody reads.
if [ "${B2D_INITRD_MODULE_FIRMWARE:-}" = yes ]; then
    grep -E '(^|/)lib/modules/[^/]+/.+\.ko(\.(xz|zst|gz))?$' "$listing" | while read -r module; do
        modinfo -F firmware "/$module" 2>/dev/null | sed "s|^|firmware $module |"
    done
fi
rm -f "$listing"
