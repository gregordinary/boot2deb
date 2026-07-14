# ASUS Chromebook C201

The `asus-c201-forky` recipe builds a bootable Debian **forky** image for the ASUS
Chromebook C201/C201PA (`google,veyron-speedy`) — an RK3288 Veyron Chromebook, and the
first 32-bit Arm board and first ChromeOS-firmware board boot2deb supports.
`asus-c201-trixie` is the same board on the stable suite.

```sh
cargo run -p boot2deb-cli -- build asus-c201-forky
```

That produces `build/asus-c201-forky/artifacts/asus-c201.img.xz` — a whole-disk image
carrying a signed kernel partition and the ext4 rootfs, so one write lays down
everything the firmware needs.

## What is unusual about this board

Almost nothing is built. The RK3288 and all ten Veyron boards are upstream, Debian's own
armhf kernel runs them, and the bootloader is not ours to make — so this recipe compiles
neither a kernel nor a bootloader, and its lock pins nothing from git:

```toml
[rootfs]
suite = "forky"
manifest = "asus-c201-forky.pkgs.lock"
```

That is the whole lock. Every package in the image — the kernel included — is pinned by
name, version, and sha256 in the manifest beside it.

**The boot payload is the kernel.** ChromeOS firmware (coreboot + depthcharge, in the
board's SPI flash) does not read a bootloader from a disk offset. It scans every boot
medium's GPT for a partition of the ChromeOS kernel type, orders the candidates by
attribute bits in the partition entry, and loads a vboot-signed FIT out of the winner.
So the image carries two partitions — a signed kernel, then the rootfs — and the
signature is built by `depthchargectl` **inside the rootfs**, deliberately: that is the
same packaged tool, reading the same `/etc/fstab`, that re-signs and re-writes the kernel
partition when `apt` upgrades the kernel on the running board.

## Board profiles

A depthcharge **board profile** describes the *firmware a unit runs*, not the board
model. The C201 supports two:

| profile | payload ceiling | for |
|---|---|---|
| `speedy` (default) | 16 MiB | stock ChromeOS firmware — **and** libreboot |
| `speedy-libreboot` | 32 MiB | a unit running libreboot, when the headroom is wanted |

The stock profile is the default deliberately: a stock-profile image boots on stock
firmware **and** on a libreboot unit, while the reverse is not true. Both are confirmed
on the hardware. Select the other with `--board speedy-libreboot`; its extra headroom is
useful for a debug initramfs carrying the display stack, which makes the boot visible on
the panel a few seconds after Ctrl+U instead of after the rootfs mounts.

## Flash and boot

Write the image to a microSD card or a USB stick:

```sh
xzcat build/asus-c201-forky/artifacts/asus-c201.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync   # confirm /dev/sdX with lsblk
```

The unit must be in **developer mode**. Then, from a full power-off, boot the medium
with **Ctrl+U** at the developer-mode screen.

- On **libreboot**, Ctrl+U works as-is.
- On **stock firmware**, external boot must first be enabled once, from a ChromeOS
  shell: `crossystem dev_boot_usb=1`.

If a boot fails, the board tells you by rebooting: the signed command line carries
`panic=30`, so a kernel panic or an initramfs that gives up on root returns to the
firmware splash about 30 seconds later. A board that *never* reboots therefore means the
kernel never reached the initramfs at all — which on a machine with no serial console is
the single most useful thing a failed boot can say. A panic also writes a full dmesg to
`BOOT2DEB-PANIC.txt` on every ext4 partition it can reach.

Expect 8-10 seconds of white screen on a healthy boot before the display comes up: the
standard image leaves the DRM stack out of the initramfs to keep the signed payload
comfortably under its ceiling, so the console appears only once the real root is mounted.

## Keyboard

This is the one shipped board with a keyboard, so it is the one that declares a console
keymap — `keymap = "us"`, the layout the C201PA ships. The RK1 and the H96 are headless
and declare none.

For a unit with another layout, either override at build time or change it on the
running board (offline, like any Debian system):

```sh
cargo run -p boot2deb-cli -- build asus-c201-forky --keymap gb
sudo dpkg-reconfigure keyboard-configuration && sudo setupcon   # on the board
```

See [Locale, timezone, and keyboard](../localization.md).

## Status

**Hardware-confirmed** on a libreboot unit: both suites, both board profiles, booting to
login with working Wi-Fi from USB via Ctrl+U.

**Stock-firmware hardware is untested.** The stock `speedy` profile is what the image
ships by default and there is good reason to expect it to work — the profile is
`depthcharge-tools`' own stock definition, the same one postmarketOS and Arch Linux ARM
use on these boards, and a libreboot unit boots it — but no one has yet booted a
boot2deb image on a C201 running its factory firmware. Treat it as high-confidence,
not proven, and note the extra `crossystem dev_boot_usb=1` step above.

Wi-Fi needs two Broadcom blobs Debian does not ship (a board NVRAM file and a Bluetooth
patchram); they are vendored in the device layer's overlay. See
`devices/asus-c201/README.md` for their provenance and why Debian's and ChromiumOS's
copies are the wrong module.

## The family

The depthcharge boot method is not C201-specific, and that is the point of it. The same
method reaches the other nine Veyron Chromebooks (each a device file and nothing else,
since their device trees are all upstream) and the RK3399 `gru` Chromebooks — which are
*easier* than this one: arm64, a 32 MiB budget, and firmware that loads a FIT ramdisk
without the DTB patching. Doing the hard 32-bit case first is what makes those nearly
free.
