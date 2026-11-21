# Radxa ZERO 3E

The Radxa ZERO 3E is a credit-card-sized board built on the Rockchip **RK3566**: four
Cortex-A55 cores, a Mali-G52 2EE GPU, and the RK3568's video blocks. boot2deb turns it
into a mainline Debian box: kernel `v7.3.5` with no patch series, and u-boot `v2026.10`
from the board's own defconfig.

| | |
| --- | --- |
| Storage | microSD only |
| Ethernet | 1x 1 GbE (SoC GMAC1, RTL8211F PHY) |
| USB | Type-C USB 2.0 OTG (also the power input), Type-C USB 3.0 host |
| Video out | micro HDMI |
| Wireless | none (the ZERO 3W is the sibling with Wi-Fi) |
| Serial console | UART2 on the 40-pin header: pin 8 TX, pin 10 RX, 1500000 baud |

The board is upstream in Linux and in u-boot, so boot2deb ships no device tree and no
u-boot patch for it. It is also the first board on the RK3566 SoC layer.

## Recipes

| Recipe | Deliverable | Status |
| --- | --- | --- |
| `radxa-zero-3e/forky` | Whole-disk Debian image (forky) | experimental: built, not booted |

It is a minimal image: Debian's standard userland with no feature selected.

```sh
boot2deb build radxa-zero-3e/forky
```

### The bootloader

u-boot builds from `radxa-zero-3-rk3566_defconfig`, which serves both the 3W and the
3E. Its `u-boot.itb` contains both boards' device trees. u-boot tells them apart at
boot by a resistor divider it reads on SARADC channel 1. A reading that matches neither
board falls back to the 3W's tree. That choice reaches only u-boot itself.
The loader hands Linux the 3E's DTB by name, so Linux gets the 3E's tree either way.

The DDR init and the BL31 are rkbin's: `rk3566_ddr_1056MHz_v1.26.bin`, and the RK3568's
`rk3568_bl31_v1.46.elf`, which rkbin pairs with both parts. The board also lists
Trusted Firmware-A as a BL31 provider (`PLAT=rk3568`). See
[The BL31 is its own axis](../reference/config-model.md#the-bl31-is-its-own-axis).

## Flash

The image is a whole-disk image for a microSD card. Press it and write it with any
raw-image flasher, as in [Producing images](../press.md):

```sh
boot2deb press radxa-zero-3e/forky card.img
lsblk    # confirm the device first; dd overwrites it whole
sudo dd if=card.img of=/dev/sdX bs=4M status=progress conv=fsync
```

The board has no eMMC, so nothing competes with the card. With no card that holds a
bootloader, the BootROM falls back to USB maskrom on the Type-C port that powers the
board.

## What the kernel drives

| Block | Driver | Notes |
| --- | --- | --- |
| Video decode | hantro | H.264 and MPEG-2 up to 1920x1088, VP8 up to 4K |
| Video encode | hantro | JPEG only |
| 2D | RGA2 (`rockchip-rga`) | V4L2 mem-to-mem |
| GPU | panfrost | Mesa from the Debian mirror, with no firmware blob |
| NPU | none | no mainline driver |

The SoC's main video decoder, VDPU346, has no mainline driver in Linux 7.3, so HEVC and
VP9 decode in software.

Mainline's device tree describes the board as an RK3566T. The CPU therefore runs at up
to 1.416 GHz and the GPU at up to 700 MHz. Radxa specifies a plain RK3566 at up to
1.6 GHz.

## Status

The image builds, and nobody has booted it on the hardware. It is `experimental` in
the [support matrix](../reference/support-matrix.md). A first boot shows whether u-boot
reports the board's full memory and whether Ethernet comes up. If you have this board,
a boot report is the one thing that would move it.
