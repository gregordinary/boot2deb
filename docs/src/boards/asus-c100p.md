# ASUS Chromebook Flip C100P

The `asus-c100p-forky` recipe builds a bootable Debian **forky** image for the ASUS
Chromebook Flip C100P/C100PA (`google,veyron-minnie`) — the 10.1" convertible of the
RK3288 Veyron family. `asus-c100p-trixie` is the same board on the stable suite.

```sh
cargo run -p boot2deb-cli -- build asus-c100p-forky
```

That produces `build/asus-c100p-forky/artifacts/asus-c100p.img.xz` — a whole-disk image
carrying two ChromeOS kernel slots and the ext4 rootfs, so one write lays down everything
the firmware needs. The kernel is in the first slot; the second ships empty, and is what
lets a later kernel upgrade roll itself back if the new kernel does not boot. See
[Upgrading the kernel](../kernel-upgrades.md).

## A C201 that folds

Structurally this board is the [C201](asus-c201.md). It includes the same
`rk3288-veyron-chromebook.dtsi`, so the EC keyboard, the trackpad, the microSD slot, the
max98090 codec and the eDP panel are all identical, and all of them — along with the
Broadcom radio, the initramfs and the network stack — are inherited from the shared layers.
Its device file states a boot method, a board profile, a DTB and a few defaults, and ships
no overlay.

Its own hardware deltas are four, and only the last two matter to you:

| | C201 | C100P |
|---|---|---|
| panel | 1366x768 `innolux,n116bge` | **1280x800 `auo,b101ean01`** |
| extra input | — | **volume buttons**, and a touchscreen |
| touchscreen | none | **Elan `ekth3500`** on `i2c3` |
| battery gauge | `sbs-battery` | **`ti,bq27500`** |

## Board profiles

One: `minnie`, which `depthcharge-tools` carries as a first-class board. There is no
libreboot port for it — the family's only libreboot profile is the C201's — so this board
runs stock ChromeOS firmware and the `crossystem` step below is required.

## Flash and boot

Write the image to a microSD card or a USB stick (the C100P has both, and two USB ports,
so unlike the [Chromebit](asus-chromebit-cs10.md) nothing here needs a hub):

```sh
xzcat build/asus-c100p-forky/artifacts/asus-c100p.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync   # confirm /dev/sdX with lsblk
```

The unit must be in **developer mode**: power off, hold **Esc + Refresh** and briefly press
**Power**, keep Esc+Refresh held until the recovery screen appears, then **Ctrl+D** and
**Enter** to confirm. It wipes and transitions; allow 15 to 20 minutes.

Then, once, from a ChromeOS shell (Ctrl+Alt+T, then `shell`):

```sh
sudo crossystem dev_boot_usb=1 dev_boot_signed_only=0
```

Reboot and press **Ctrl+U** at the "OS verification is OFF" screen. Ctrl+U covers the SD
card as well as USB.

The convertible's side buttons do nothing here. The recovery combo is read by the EC from
the built-in keyboard, and the volume-button sequence Google documents belongs to
detachables and tablets, which this firmware is not built as. Use Esc+Refresh+Power.

A USB keyboard will not help you at these screens either: `CONFIG_LP_USB_HID` is not set in
this board's libpayload, so depthcharge reads the EC keyboard and nothing else. (The
Chromebit, which has no EC, is the one board in the family built the other way.)

If a boot fails, the board tells you by rebooting: the signed command line carries
`panic=30`, so a kernel panic or an initramfs that gives up on root returns to the firmware
splash about 30 seconds later. A board that *never* reboots means the kernel never reached
the initramfs at all. A panic also writes a full dmesg to `BOOT2DEB-PANIC.txt` on every
ext4 partition it can reach.

Expect 8-10 seconds of white screen on a healthy boot before the display comes up: the
standard image leaves the DRM stack out of the initramfs to keep the signed payload
comfortably under its 16 MiB ceiling, so the console appears only once the real root is
mounted.

## Installing to the eMMC

The board has 16 GB of internal eMMC, and the image is a whole-disk image, so putting the
OS there is one command from a booted card:

```sh
lsblk                       # the eMMC is mmcblk0 — the one with mmcblk0boot0 beside it
xzcat asus-c100p-forky.img.xz | sudo dd of=/dev/mmcblk0 bs=4M status=progress conv=fsync
sudo reboot                 # Ctrl+D boots the eMMC, Ctrl+U the card
```

This needs no kernel patch, contrary to the usual advice. The Veyron eMMC ships with its
primary GPT deliberately corrupted — ChromeOS marks it `IGNOREME` and uses the secondary,
and a stock kernel cannot read a table like that. That only bites if you *keep* the factory
GPT. Writing a whole-disk image lays down a fresh, valid one over the top, which a stock
kernel reads like any other.

## The touchscreen and the battery gauge do not work

Both are gaps in **Debian's kernel configuration**, not in the hardware, the boot path, or
this board's device tree. The C100P needs two drivers that Debian's armhf kernel does not
build, in forky (7.1.3) and trixie (6.12.94) alike:

| what | driver | Debian armhf |
|---|---|---|
| Elan `ekth3500` touchscreen | `elants_i2c` | `# CONFIG_TOUCHSCREEN_ELAN is not set` |
| `ti,bq27500` fuel gauge | `bq27xxx_battery_i2c` | `# CONFIG_BATTERY_BQ27XXX_I2C is not set` |

So a C100P image comes up with a working keyboard, trackpad, panel, HDMI, Wi-Fi,
Bluetooth and audio — and no touch input and no battery percentage.

Neither is a near miss you can work around in config. The modules are simply absent from
the kernel Debian ships. Note the trackpad is unaffected and does work: it is a
*different* Elan driver (`elan_i2c`, `CONFIG_MOUSE_ELAN_I2C=m`), and the similar names are
the only thing the two have in common. The C201 is unaffected by both gaps — its battery is
an SBS one, which Debian does build, and it has no touchscreen.

## Keyboard

A laptop, so it declares a console keymap — `keymap = "us"`, the layout the C100PA ships.

```sh
cargo run -p boot2deb-cli -- build asus-c100p-forky --keymap gb
sudo dpkg-reconfigure keyboard-configuration && sudo setupcon   # or, on the board
```

See [Locale, timezone, and keyboard](../localization.md).

## Getting online

There is no ethernet port, so Wi-Fi is the only way onto the network:

```sh
sudo nmtui        # pick "Activate a connection", choose the network, enter the key
```

The radio is the family's Broadcom BCM4354 and needs two blobs Debian does not ship; they
are vendored on the SoC layer and are already in the image. Bluetooth works as it does on
the C201 — the BCM4354's Bluetooth half is on `uart0`, the kernel loads the vendored
patchram, and `bluez` is installed to use it. `btsdio` is blacklisted, because if it claims
the BCM4354's SDIO Bluetooth function, Wi-Fi does not survive suspend and resume.

## Audio

The same max98090 as the C201, so the same first-boot fixup applies unchanged. The codec
comes up with its amplifiers muted *and* the DAPM mixers that feed them holding their DAC
input switches open — so there is no route from the DAC to the speakers to unmute in the
first place, and clearing only the obvious `Speaker` control leaves the board silent.

The SoC layer's `first-boot.d/20-audio` hook closes the routing switches, unmutes both
amplifiers, sets sane volumes, and runs `alsactl store`; `alsa-utils` replays the result on
every later boot. Adjust it like any other Debian system:

```sh
alsamixer && sudo alsactl store
```

## Display

A 1280x800 eDP panel and a micro-HDMI port, both driven by mainline `rockchip-drm`.

The panel's backlight has one quirk worth knowing if you write to it directly: its PWM duty
must be at least 1%, so the device tree starts its brightness scale at **3, not 0**. A
userspace policy that writes 0 to turn the backlight down is doing something this panel does
not accept.

HDMI does **4K30** and cannot do 4K60 — the RK3288 caps TMDS at 340 MHz, its PHY has no
scrambling above that, and the VOP cannot emit YUV420, so there is no reduced-rate path.
Nothing in the image configures any of this.

Like the C201, this board lights two display controllers, and the smaller one (VOPL) tops
out at 2560x1600 while advertising the same maximum as the larger. Which one the HDMI
encoder lands on is decided at runtime by DRM, not by configuration; `dmesg | grep -i vop`
says which it got. That is the thing to check if a 4K display comes up showing only part of
the picture.

## Status

**Not yet booted on hardware.** The image builds, and everything it is made of is shared
with a board that does boot: the C100P resolves to the same rootfs, the same boot method,
the same signed-payload flow, the same initramfs and the same Debian kernel as the C201,
which is confirmed booting to a login prompt. It differs from it in a DTB, a depthcharge
profile and a hostname.

Of the family's three boards this is the one most likely to boot first time — it is the
C201 in a different case, and unlike the Chromebit it has a card slot, a keyboard and two
USB ports, so there is nothing unusual about getting an image into it. The known gaps are
the touchscreen and the battery gauge, above; audio and Bluetooth ship configured and are
unverified here.

## The family

The depthcharge boot method is not board-specific, and this is what that buys: the C100P is
a device file and nothing else. No overlay, no kernel, no engine change — its device tree is
upstream and everything that makes a Veyron boot lives on the shared layers. The same holds
for the [Chromebit CS10](asus-chromebit-cs10.md), and for the seven Veyron boards not yet
written.
