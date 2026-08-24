# ASUS Chromebook C201

The `asus-c201/forky` recipe builds a bootable Debian **forky** image for the ASUS
Chromebook C201/C201PA (`google,veyron-speedy`) — an RK3288 Veyron Chromebook, and the
first 32-bit Arm board and first ChromeOS-firmware board boot2deb supports.

| Recipe | Kernel | Status |
| --- | --- | --- |
| `asus-c201/forky` | Debian's `armmp`, installed from the archive | validated |
| `asus-c201/trixie` | the same, on the stable suite | expected |
| `asus-c201/mainline-forky` | compiled here from mainline 7.1.y | validated |

The first two are the board as Debian ships it. The third is the seam for changing the
kernel — see [A kernel of your own](#a-kernel-of-your-own) below.

```sh
boot2deb build asus-c201/forky
```

That produces `build/asus-c201/forky/artifacts/asus-c201-forky.img.xz` — a whole-disk image
carrying a signed kernel partition and the ext4 rootfs, so one write lays down
everything the firmware needs.

## What is unusual about this board

Almost nothing is built. The RK3288 and all ten Veyron boards are upstream, Debian's own
armhf kernel runs them, and the bootloader is not ours to make — so `asus-c201/forky`
compiles neither a kernel nor a bootloader, and its lock pins nothing from git:

```toml
[rootfs]
suite = "forky"
manifest = "forky.pkgs.lock"
```

That is the whole lock. Every package in the image — the kernel included — is pinned by
name, version, and sha256 in the manifest beside it.

**The boot payload is the kernel.** ChromeOS firmware (coreboot + depthcharge, in the
board's SPI flash) does not read a bootloader from a disk offset. It scans every boot
medium's GPT for a partition of the ChromeOS kernel type, orders the candidates by
attribute bits in the partition entry, and loads a vboot-signed FIT out of the winner.
The signature is built by `depthchargectl` **inside the rootfs**, deliberately: that is
the same packaged tool, reading the same `/etc/fstab`, that re-signs and rewrites a kernel
partition when `apt` upgrades the kernel on the running board.

So the image carries **three** partitions: two ChromeOS kernel slots and the rootfs.

| # | partition | contents |
|---|---|---|
| 1 | `KERN-A`, 16 MiB @ 12 MiB | the signed kernel |
| 2 | `KERN-B`, 16 MiB @ 28 MiB | empty, priority 0 — the upgrade spare |
| 3 | rootfs, ext4 @ 44 MiB | grown to fill the medium on first boot |

The empty second slot is not waste. It is what makes a kernel upgrade on this board
atomic and reversible: `apt` writes the slot the board is *not* running from, and if the
new kernel fails to boot, the firmware falls back to the old one by itself. See
[Upgrading the kernel](../kernel-upgrades.md).

## A kernel of your own

`asus-c201/mainline-forky` is the same board and the same suite, with the kernel compiled
here from **mainline 7.1.y** instead of installed from the archive. Its kconfig comes from
the fragments — a Debian-parity baseline plus the RK3288 slice — on top of the in-tree
`multi_v7_defconfig`, and the `rk3288-fixes` patch series rides along. Everything else is
identical: the same signed-kernel boot payload, the same board profiles, the same
`combined` layout.

```sh
boot2deb build asus-c201/mainline-forky
```

That series currently carries one patch, and it is a good illustration of why the recipe
exists. RK3288's `crypto/rk_crypto` hashes do not inherit their fallback's `statesize`,
so any user of the shash export/import path gets a truncated state; the fix is four lines,
and nothing about it is board-specific — it is simply not in a released kernel yet. A
compiled recipe is where a fix like that lives until it is.

The trade is what you would expect. This recipe compiles a kernel, so a cold build
provisions a cross root and takes real time, where `asus-c201/forky` is a rootfs
bootstrap and an image assembly. It also takes on the maintenance Debian was doing for
you: a `7.1.y` point release is an `update --kernel-ref` and a rebuild, not an
`apt upgrade`. Take it when you need a kernel change; stay on `asus-c201/forky` when you
do not.

## Board profiles

A depthcharge **board profile** describes the *firmware a unit runs*, not the board
model. The C201 has two, and each is a whole shipped build point:

| build | profile | payload | initrd delivery | boots on |
|---|---|---|---|---|
| `asus-c201/forky` | `speedy` | 16 MiB slots | patched into every DTB's `/chosen` | stock firmware **and** libreboot |
| `asus-c201-libreboot/forky` | `speedy-libreboot` | 32 MiB slots | a real FIT ramdisk node | libreboot only |

Both are confirmed on the hardware. **The stock profile is the default deliberately**: a
stock-profile image boots on either firmware, while the reverse is not true —
depthcharge-tools sets `hwid-match = None` on the libreboot profile, so a running
`depthchargectl` on a libreboot unit resolves back to plain `speedy` and applies the
stock constraints.

The libreboot build exists for the payload headroom: libreboot's depthcharge buffers a
32 MiB kernel (`CONFIG_KERNEL_SIZE = 0x2000000`) where the stock firmware buffers 16,
which is room for a debug initramfs carrying the display stack — making the boot visible
on the panel a few seconds after Ctrl+U instead of after the rootfs mounts. Taking that
headroom needs a matching kernel *partition*, not just the profile, so the two travel
together on `devices/asus-c201-libreboot.toml`: it `extends = "asus-c201"` and states
only the profile and `kpart_size = "32MiB"`. Everything else — the DTB, the kernel set,
the keymap, the overlay tree — is the C201's and is inherited.

The wider slots move the rootfs from 44 MiB to 76 MiB into the image, which is the whole
cost. Build it like any other recipe:

```sh
boot2deb build asus-c201-libreboot/forky
```

## Flash and boot

Write the image to a microSD card or a USB stick:

```sh
xzcat build/asus-c201/forky/artifacts/asus-c201-forky.img.xz \
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

A laptop, so it declares a console keymap — `keymap = "us"`, the layout the C201PA ships.
The RK1 and the H96 are headless and declare none.

Note that a USB keyboard is *not* an option at the firmware screens on this board:
`CONFIG_LP_USB_HID` is not set in its libpayload, so depthcharge reads Ctrl+U from the EC
keyboard and nothing else. (The [Chromebit](asus-chromebit-cs10.md), which has no EC, is
the one board in the family built the other way.)

There are two ways to get another layout, and neither is a `build` flag — an image's
keymap comes from the config its lock was resolved against:

- **Change it on the running board**, offline, like any Debian system:

  ```sh
  sudo dpkg-reconfigure keyboard-configuration && sudo setupcon
  ```

- **Bake it into an image** by writing a recipe that sets `keymap`. `resolve` shows what
  a choice resolves to before you commit it, and names the file to write:

  ```sh
  boot2deb resolve asus-c201/forky --keymap gb
  ```

  See [Adapting a shipped recipe](../tutorials/adapting-a-recipe.md) and
  [Locale, timezone, and keyboard](../localization.md).

## Getting online

There is no ethernet port, so Wi-Fi is the only way onto the network and joining one is
the first thing to do after logging in:

```sh
sudo nmtui        # pick "Activate a connection", choose the network, enter the key
```

NetworkManager owns the interfaces (the base layer's `dhcpcd` is excluded here, so the
two do not fight over the NIC), and it remembers the network, so this is a one-time
step. `nmcli device wifi list` and `nmcli device wifi connect <ssid> --ask` do the same
job without the interface.

Wi-Fi needs two Broadcom blobs Debian does not ship; they are vendored in the device
layer and are already in the image. Scanning shows randomized, locally-administered MAC
addresses — that is NetworkManager, not a fault.

## Audio

The image comes up with working speakers. That takes a little doing, because the
max98090 codec starts in a state where two separate things are in the way: its
amplifiers are muted, *and* the DAPM mixers that feed them have their DAC input
switches open, so there is no route from the DAC to the speakers to unmute in the first
place. Clearing only the mutes — which is what reaching for the obvious `Speaker`
control does — still leaves the board silent.

The device's `first-boot.d/20-audio` hook closes the routing switches, unmutes both
amplifiers, sets sane volumes, and runs `alsactl store`. `alsa-utils` replays the
result on every later boot, so this happens once and then it is simply the board's
mixer state. Adjust it like any other Debian system:

```sh
alsamixer && sudo alsactl store
```

## Bluetooth

The Wi-Fi and Bluetooth halves of the BCM4354 arrive on different buses: Wi-Fi over
SDIO, Bluetooth over `uart0`, which the device tree wires as `brcm,bcm43540-bt`. The
kernel loads the Bluetooth patchram this device vendors alongside the Wi-Fi NVRAM, and
the image ships `bluez` so there is a host stack to use it.

`btsdio` is blacklisted. The BCM4354's SDIO side also advertises a Bluetooth function,
and if `btsdio` claims it, Wi-Fi does not survive suspend and resume.

## Display

An eDP panel and a real HDMI port, both driven by mainline `rockchip-drm`.

HDMI does **4K30** (3840x2160 at a 297 MHz pixel clock) and cannot do 4K60. That is the
hardware: the RK3288 caps TMDS at 340 MHz, its HDMI PHY has no scrambling above that,
and the VOP cannot emit YUV420, so there is no reduced-rate path to 4K60 either. Nothing
in the image configures any of this — the ceilings are constants in the driver, and the
kernel Debian ships already supports everything the SoC can do.

One quirk is worth knowing if a 4K display comes up showing only part of the picture.
The RK3288 has two display controllers, and the smaller one (VOPL) tops out at 2560x1600
while advertising the same maximum as the larger one. Which controller the HDMI encoder
lands on is decided at runtime by DRM, not by configuration. `dmesg | grep -i vop` says
which one it got.

## Status

**A boot2deb-built image boots this board.** Confirmed end to end on a libreboot unit,
from USB via Ctrl+U: forky comes up, runs first boot, and reaches a login prompt. The
per-image password works and is changed at first login; `nmtui` joins Wi-Fi.

The `root=` baked into the *signed* kernel names the rootfs PARTUUID the image was built
with, and the device keeps that identity for life — first boot grows the partition but
never rewrites its PARTUUID, so the signature stays valid with nothing to re-sign. KERN-A
ships signed and correct; the empty KERN-B spare is first populated by a kernel upgrade,
which writes the slot it is not running from and leaves the proven one as the fallback.

**The compiled-kernel recipe boots too.** An `asus-c201/mainline-forky` image came up on
the same unit with no self-test failures, no warnings, and taint 0 — so the mainline 7.1.y
kernel and the `rk3288-fixes` patch are both proven on silicon. A later build of it
also booted from USB and **installed cleanly to internal eMMC**, which exercises the whole
image path rather than just the boot.

Expect a **white screen for roughly 10 seconds** after Ctrl+U before the boot messages
appear. That is normal and not a fault: there is no display driver in the initramfs, so
the panel holds the firmware's last frame until the kernel's DRM stack takes over.

Audio is **confirmed on hardware**: the internal speakers and volume control work out
of the box. Bluetooth ships configured — the kernel log shows the radio initialize and
load its patchram — but has not yet been exercised against a device.

**Stock-firmware hardware is untested.** The stock `speedy` profile is what the image
ships by default and there is good reason to expect it to work — the profile is
`depthcharge-tools`' own stock definition, the same one postmarketOS and Arch Linux ARM
use on these boards, and a libreboot unit boots it — but no one has yet booted a
boot2deb image on a C201 running its factory firmware. Treat it as high-confidence,
not proven, and note the extra `crossystem dev_boot_usb=1` step above.

Wi-Fi needs two Broadcom blobs Debian does not ship (a board NVRAM file and a Bluetooth
patchram); they are vendored in the SoC layer's overlay, since it is the same radio module
on every Broadcom board in the family. See `socs/rk3288/README.md` for their provenance
and why Debian's and ChromiumOS's copies are the wrong module.

## The family

The depthcharge boot method is not C201-specific, and that is the point of it. Two
siblings ship already — the [C100P](asus-c100p.md) and the
[Chromebit CS10](asus-chromebit-cs10.md) — and each is a device file and nothing else: no
overlay, no engine change, no kernel. Their device trees are upstream and everything that
makes a Veyron boot lives on the shared layers.

The same method reaches the seven remaining Veyron boards and the RK3399 `gru`
Chromebooks — which are *easier* than this one: arm64, a 32 MiB budget, and firmware that
loads a FIT ramdisk without the DTB patching. Doing the hard 32-bit case first is what
makes those nearly free.
