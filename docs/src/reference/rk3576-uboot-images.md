# RK3576 u-boot images

RK3576 boards build one of three u-boot images from mainline u-boot on the
`rk3576-generic` control DTB. They share the same SoC bring-up — the clock and
timer fixes, the inno-usb2 PHY reset, the VOP quiesce at OS handoff — and differ
in how far each goes beyond booting:

| Image | Role |
|---|---|
| **loader** | Flash and dump a board from a laptop. |
| **display** | The u-boot an image ships with. Boots, and recovers without a serial cable. |
| **util** | A recovery and bring-up tool: a boot menu, diagnostics, and image verification at the prompt. |

The u-boot variant is its own axis: a recipe selects one with `uboot_series`,
independent of the kernel. A `loader`/`util` tool is a u-boot-only deliverable
(`deliverable = "uboot"`) that names no suite or kernel; `display` is the u-boot
a full image recipe ships with.

### Generic vs board-specific

Most of the RK3576 u-boot series is SoC-generic — it patches only the
`rk3576-generic` control DTB and defconfig, so the payload is identical on any
RK3576 board. Those images are therefore homed on the **`rk3576-generic`** tool
host (not a board):

| Image | Recipe | u-boot series | Scope |
|---|---|---|---|
| loader | `rk3576-generic/loader` | `rk3576-loader` | SoC-generic |
| util | `rk3576-generic/util` | `rk3576-util` | SoC-generic |

A board contributes its own recipes where it needs something board-specific:

| Image | Recipe | u-boot series | Scope |
|---|---|---|---|
| display (shipped image) | `h96-max-m9/forky` | `rk3576-display` | board image |
| util + ethernet | `h96-max-m9/util` | `h96-max-m9-util` | board-specific |

`h96-max-m9/util` is the SoC-generic util plus the H96's GMAC0 RGMII ethernet,
so a rescue session can pull an image over the network (`dhcp`/`tftp`) and
`ping`. The ethernet is board-specific — an RTL8211F at MDIO address 1, PHY
reset on gpio2 PB3, `tx_delay 0x1b`, `rgmii-rxid` — so it ships in a board
series (`h96-max-m9-util`) that layers one board patch on the generic util
series, leaving the SoC-generic images ethernet-free. The board patch adds the
nodes to the shared `rk3576-generic` control DTB; a board wanting its own
control DTB would carry the ethernet there instead.

## USB port roles

RK3576 exposes two USB controllers, and u-boot has no runtime OTG role switch on
them, so each build fixes their roles at build time:

- **drd0** (the USB 3.0 OTG port) is a **device**. It is the rockusb/ums gadget
  and the port the BootROM download cable uses — a laptop connects here.
- **drd1** (the USB 2.0 host port) is a **host**, for a keyboard and a
  USB stick.

A single build is therefore both a device (to a laptop, on drd0) and a host (for
peripherals, on drd1) at once, one role per connector. A USB hub on the drd1 port
carries a keyboard and a bootable stick together — u-boot enumerates hubs during
`usb start` and boots from a device behind one.

The images with a USB keyboard (display and util) run `usb start` automatically
before the prompt (`USE_PREBOOT`, whose default command is `usb start` once
`USB_KEYBOARD` is set), so the keyboard is live at the prompt with nothing typed
over serial — which is what lets these images be driven with no UART at all. This
enumerates only the drd1 host; drd0 stays the gadget, so `ums` and
`reboot loader`→rockusb are unaffected.

The loader image is the exception: it brings up no USB host and no display, so
drd0 is its only USB role.

## Capabilities

| | loader | display | util |
|---|:---:|:---:|:---:|
| Delivery | maskrom RAM | eMMC (shipped) | maskrom RAM / flashable |
| Autoboots | — | 10 s, interruptible | — (drops to the prompt) |
| Serial console | yes | yes | yes |
| HDMI console + USB keyboard | — | yes | yes |
| `ums` (export a block device to a laptop) | yes | yes | yes |
| rockusb via `reboot loader` | yes | yes | yes |
| SARADC download key → BootROM | yes | yes | yes |
| maskrom USB boot images (usb471/472) | yes | yes | yes |
| Boot a USB rescue stick (`bootflow scan`, extlinux) | yes | yes | yes |
| `md` / `mw` (memory peek/poke) | yes | yes | yes |
| `bootmenu` (interactive boot menu) | — | — | yes |
| `clk` (dump the clock tree) | — | — | yes |
| `memtest` (DRAM walk) | — | — | yes |
| `md5sum` / `sha1sum` (verify an image) | — | — | yes |
| `smc` / cache commands (developer) | — | — | yes |
| ethernet (`dhcp`/`tftp`/`ping`) | — | — | board util only |

The SoC-generic images bring up no networking; recovery runs over USB and
maskrom. A board's own util recipe can add ethernet — `h96-max-m9/util` does,
and it is validated on the H96 (DHCP binds a lease and `ping` replies).

## Building

Each image's u-boot is produced by staging just the bootloader:

```sh
boot2deb build <recipe> --stage uboot
```

For the maskrom-delivered images (loader and util), the deliverable is the
`u-boot-rockchip-usb471.bin` / `usb472.bin` pair (and the packed
`u-boot-rockchip-maskrom.bin`) — stream them into RAM with the BootROM download
protocol and run u-boot with nothing written to storage. See
[The maskrom loader](maskrom-loader.md).

For the display image, the u-boot is written to the raw gap ahead of the rootfs
as part of a full image build, and also emitted on its own by `--stage uboot`
for reflashing.

## Choosing an image

- **display** ships on the board. A user whose OS will not boot reaches the
  u-boot prompt on the television with a USB keyboard, boots a rescue stick, or —
  with a laptop on drd0 — runs `ums` to image the eMMC or `reboot loader` to
  re-flash over rockusb.
- **util** is the same hardware support with a boot menu and the diagnostic and
  verification commands, and it never autoboots. It is normally streamed into RAM
  over maskrom for a recovery or bring-up session rather than installed. The
  SoC-generic `rk3576-generic/util` is the board-neutral tool; a board that has
  ethernet wired gets a board util recipe (`h96-max-m9/util`) that adds it, so a
  rescue session can also `dhcp` and pull a rescue image over `tftp`.
- **loader** is the minimal laptop-driven path: no host and no display, just
  rockusb and `ums` on drd0. It is the smallest image that can flash or dump a
  board.
