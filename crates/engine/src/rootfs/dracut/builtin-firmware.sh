#!/bin/bash
# The firmware of the drivers built into the kernel, for an image whose initrd dracut
# builds. boot2deb installs it as a dracut module; dracut sources it.
#
# dracut installs a driver's firmware together with the driver's module, so a driver
# built into the kernel gets none. A built-in driver still probes while the initrd is
# the root filesystem, as soon as what it waits on is there, and request_firmware()
# then reads the initrd. The RK3588's panthor is one: the PMIC modules the initrd loads
# bring up its regulator, and with no CSF firmware in the initrd its probe fails and
# the GPU never appears. initramfs-tools installs these files from
# modules.builtin.modinfo; this module does the same for dracut.
#
# Each listed file is installed in the first spelling the kernel's loader opens: the
# bare name in each firmware directory, then .zst, then .xz. A listed file the image
# does not have is a warning, as under initramfs-tools, since a driver lists one file
# per chip it supports. A file that is there and fails to install fails the build.
#
# The list is read in bash rather than through tr, so it means the same under GNU and
# uutils coreutils.

# Always included: what it installs follows the kernel, not the build host.
check() {
    return 0
}

installkernel() {
    local modinfo="$srcmods/modules.builtin.modinfo"
    local entry mod fw ext dir found
    if [[ ! -r $modinfo ]]; then
        dwarn "no $modinfo, so no firmware of a built-in driver is installed"
        return 0
    fi
    # One NUL-terminated `<module>.<key>=<value>` entry per field; a module name has
    # no dot, so the key starts after the first one.
    while IFS= read -r -d '' entry || [[ $entry ]]; do
        [[ ${entry#*.} == firmware=* ]] || continue
        mod=${entry%%.*}
        fw=${entry#*.firmware=}
        found=
        for ext in "" .zst .xz; do
            for dir in "/lib/firmware/updates/$kernel" /lib/firmware/updates \
                "/lib/firmware/$kernel" /lib/firmware; do
                if [[ -e ${dracutsysrootdir-}$dir/$fw$ext ]]; then
                    found=$dir/$fw$ext
                    break 2
                fi
            done
        done
        if [[ $found ]]; then
            inst_simple "$found" || return 1
        else
            dwarn "no firmware $fw for the built-in driver $mod"
        fi
    done < "$modinfo"
}
