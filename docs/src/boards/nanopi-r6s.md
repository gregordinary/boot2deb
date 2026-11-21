# NanoPi R6S and R6C

The FriendlyElec NanoPi R6S and R6C are small routers built on the Rockchip **RK3588S**,
the RK3588's smaller package. It has the same 4x Cortex-A76 + 4x Cortex-A55 CPU, Mali-G610
GPU, NPU, video decoders, encoder and RGA, with fewer I/O lanes. boot2deb turns either
board into a mainline Debian box: kernel `v7.3.4`, u-boot `v2026.10`, and Trusted
Firmware-A as the BL31.

| | R6S | R6C |
| --- | --- | --- |
| Ethernet | 2x 2.5 GbE (RTL8125B, PCIe) + 1x 1 GbE (SoC GMAC, RTL8211F PHY) | 1x 2.5 GbE (RTL8125B, PCIe) + 1x 1 GbE (SoC GMAC, RTL8211F PHY) |
| M.2 | none | M-key, NVMe (PCIe 2.0 x1) |
| Storage | eMMC, microSD | eMMC (on most fittings), microSD, the M.2 slot |
| Other | HDMI out, USB 3.0 + USB 2.0 Type-A, MASK and user buttons | the same |

Both boards are upstream in Linux and in u-boot, so neither ships a device tree or a
board patch of boot2deb's. The kernel is the [Turing RK1](turing-rk1.md)'s, with the
same `rk3588-accel` patch series. The R6C is a variant of the R6S in the config tree
(`extends = "nanopi-r6s"`). The two device trees include one shared board file, and
differ in a single LED label.

u-boot is upstream's plus one SoC fix, the `rk3588-uboot-fixes` series. With it, SPL
sets SPLL, the clock of the SoC's center interconnect, on every boot device. Upstream
v2026.10 sets it only on a boot from microSD. The fix is upstream from v2027.01.

## Recipes

| Recipe | Deliverable | Status |
| --- | --- | --- |
| `nanopi-r6s/forky` | Whole-disk Debian image (forky), BL31 from Trusted Firmware-A | experimental: built, not booted |
| `nanopi-r6s/efi-forky` | The same, booted through UEFI: GRUB on an EFI system partition | experimental: built, not booted |
| `nanopi-r6c/forky` | The same as `nanopi-r6s/forky`, for the R6C | experimental: built, not booted |

Each is a minimal image: Debian's standard userland with no feature selected. The
kernel contains the video, RGA and NPU drivers, and the Rockchip media userspace is not
installed.

```sh
boot2deb build nanopi-r6s/forky
```

### UEFI boot

`nanopi-r6s/efi-forky` selects the `efi` loader. u-boot starts GRUB from the image's EFI
system partition, at the removable-media path, and GRUB boots the kernel on the rootfs.
u-boot hands the kernel the board DTB the image places on the same partition, at
`\dtb\rockchip\rk3588s-nanopi-r6s.dtb`: the path u-boot's own `fdtfile` names. Every
other pin is the forky recipe's. See
[The loader is its own axis](../reference/config-model.md#the-loader-is-its-own-axis).

On the board, u-boot's console shows it scanning the partition and starting
`/EFI/BOOT/BOOTAA64.EFI`, then GRUB's menu, which waits five seconds before booting the
newest kernel. `boot2deb-selftest` checks the loader and the DTB on the partition beside
everything else, once `/boot/efi` is mounted.

### The BL31

The recipes build the BL31 from Trusted Firmware-A's source, at the ref the RK3588 SoC
layer names, in place of rkbin's prebuilt blob. The DDR init is rkbin's either way. The
boards list both providers, and default to rkbin, so a recipe that omits `bl31` (or sets
`bl31 = "rkbin"`) packs rkbin's BL31 v1.56 instead. See
[The BL31 is its own axis](../reference/config-model.md#the-bl31-is-its-own-axis).

The DDR init this SoC ships with, v1.24, is the one rkbin pairs with BL31 v1.53 or later.
Whether Trusted Firmware-A meets the same requirement is what a first boot on TF-A shows.
Compare the memory u-boot reports with the board's fitting, and check `/proc/meminfo`
once Linux is up. A board that comes up with half its memory, or not at all, boots on an
rkbin build.

## Flash

The image is a whole-disk image for the eMMC or a microSD card. Press it and write it
with any raw-image flasher, as in [Producing images](../press.md):

```sh
boot2deb press nanopi-r6s/forky card.img
lsblk    # confirm the device first; dd overwrites it whole
sudo dd if=card.img of=/dev/sdX bs=4M status=progress conv=fsync
```

The BootROM reads the eMMC before the microSD slot. A board whose eMMC holds a
bootloader, as the factory FriendlyWrt image does, boots that and never reads the card.
Clear it first: from the system on the eMMC, zero the eMMC's first 16 MiB, the raw gap
that holds the idbloader and `u-boot.itb`. The eMMC is the `/dev/mmcblkN` with
`mmcblkNboot0` and `mmcblkNboot1` siblings. Its partition table goes with it, so the
system on it does not boot again. The next power-on falls through to the card.

### Installing to the eMMC from the card

Press the card with `--embed-image`, which places the recipe's own compressed image
inside it, and the booted card writes the eMMC itself:

```sh
boot2deb press nanopi-r6s/forky card.img --embed-image \
    --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
```

Boot the card, let first boot finish, then:

```sh
sudo boot2deb-install-to /dev/mmcblkN
```

`boot2deb-install-to` ships in every image. It checks the embedded image against the
record pressed beside it. It refuses anything that is not a whole disk, and the disk the
system is running from. It refuses a disk with anything mounted on it, and one too small
for the image. It then requires you to type the device's name. It reads the disk back after
the write, before it reports the install done.

Power off, take the card out, and the board boots from the eMMC, whose rootfs grows to
fill it on that first boot.

The MASK button puts the board in maskrom mode, where a USB flasher with a Rockchip
loader writes the eMMC directly. It is the route back from an eMMC whose bootloader does
not start.

## Serial console

The console is UART2 at **1500000 baud**, 8N1, not 115200. That is a Rockchip default
which will otherwise look like a dead port. u-boot, the BL31 and the kernel all print
there.

## First boot

Power on. The image regenerates its SSH host keys and grows the rootfs to fill the
medium, online, in the same boot. Log in as **`debian`** with the password the build
printed. It is expired, so you set a new one immediately. `sudo` asks for that password,
and the hostname is `nanopi-r6s` (`nanopi-r6c` on the R6C).

Every wired port takes an address over DHCP, so plug a cable into any of them. u-boot
derives the SoC port's MAC address from the CPU ID and writes it into the device tree.
The address is therefore the same on every boot. The RTL8125B ports load their firmware
from `firmware-realtek`, which the image installs.

`boot2deb-selftest` checks what the RK3588 SoC layer and the kernel expect. The GPU is
bound to panthor with its firmware, and the kernel log has no bus fault. The device
nodes are present: the decoder's `/dev/video0`, `/dev/rga`, `/dev/mpp_service` and the
NPU's `/dev/accel/accel0`.

## Status

Every recipe here is `experimental` in the
[support matrix](../reference/support-matrix.md): the images build, and no image from
them has booted yet. The first boot on each board is what moves them, and the selftest
plus the memory check above are what it takes.
