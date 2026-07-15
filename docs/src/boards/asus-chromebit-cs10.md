# ASUS Chromebit CS10

The `asus-chromebit-cs10-forky` recipe builds a bootable Debian **forky** image for the
ASUS Chromebit CS10 (`google,veyron-mickey`) — an RK3288 Veyron, like the
[C201](asus-c201.md) and the [C100P](asus-c100p.md), but in an HDMI stick rather than a
laptop. `asus-chromebit-cs10-trixie` is the same board on the stable suite.

```sh
cargo run -p boot2deb-cli -- build asus-chromebit-cs10-forky
```

That produces `build/asus-chromebit-cs10-forky/artifacts/asus-chromebit-cs10.img.xz` — a
whole-disk image carrying two ChromeOS kernel slots and the ext4 rootfs, so one write lays
down everything the firmware needs. The kernel is in the first slot; the second ships
empty, and is what lets a later kernel upgrade roll itself back if the new kernel does not
boot. See [Upgrading the kernel](../kernel-upgrades.md).

## What is unusual about this board

Its device tree tells you the whole story in one line. `rk3288-veyron-mickey.dts`
includes `rk3288-veyron.dtsi` **directly** and never picks up
`rk3288-veyron-chromebook.dtsi` — the file that gives every other board in the family its
SD slot, its ChromeOS EC, its keyboard, its trackpad, its eDP panel and its max98090
codec. The Chromebit has none of those, and that single omission is what the board *is*:

| | Chromebit | the Veyron laptops |
|---|---|---|
| removable storage | **none** — no SD slot | microSD |
| internal storage | 16 GB eMMC (`mmcblk0`) | 16 GB eMMC |
| keyboard | **none** — USB only | EC keyboard on `spi0` |
| USB ports | **one** | two, plus the card slot |
| display | HDMI only | eDP panel + HDMI |
| audio | HDMI only (`ROCKCHIP-HDMI`) | max98090 (`ROCKCHIP-MAX98090-HDMI`) |
| battery / lid | none — mains only | both |

Everything *else* is the family's and is inherited unchanged: the Broadcom radio, the
initramfs, the network stack, the boot method. So for all that it is the odd one out, the
board's device file states a boot method, a board profile, a DTB and a handful of
defaults — and ships no overlay at all.

## Board profiles

One: `mickey`, which `depthcharge-tools` carries as a first-class board. There is no
libreboot port for the Chromebit — the family's only libreboot profile is the C201's —
so **this board always runs stock ChromeOS firmware**, and the `crossystem` step below
is not optional the way it is on a libreboot C201.

## You will need a USB hub

The Chromebit has **one USB 2.0 port**, and installing needs two things in it at once: a
keyboard, to press Ctrl+U, and the stick you are booting. So a hub is not a convenience
here, it is a prerequisite.

It does work, and not by luck. The Chromebit is the **one board in the Veyron family
whose firmware is built with USB HID** — `CONFIG_DRIVER_INPUT_USB=y` in depthcharge, and
`CONFIG_LP_USB_HID=y` plus `CONFIG_LP_USB_HUB=y` in its libpayload — because it is the
one board with no keyboard to read instead. A keyboard behind a hub is precisely the
topology this firmware was compiled for. (The inverse is also true and worth knowing:
`CONFIG_LP_USB_HID` is **not** set on the C201 or the C100P, so on *those* boards a USB
keyboard does nothing at the firmware screens.)

Use a **self-powered** hub, with its own wall supply. ASUS's own documentation warns that
anything drawing more than 500 mA should not hang off this port directly, and a keyboard
plus a flash drive on a bus-powered hub is a real brown-out risk.

## Flash and boot

Write the image to a USB stick:

```sh
xzcat build/asus-chromebit-cs10-forky/artifacts/asus-chromebit-cs10.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync   # confirm /dev/sdX with lsblk
```

**Entering developer mode.** The Chromebit has no power button — it boots the instant DC
is applied — so the usual "hold keys and tap power" does not exist here. The sequence is:

1. Connect HDMI, then the powered hub, then a **wired USB keyboard** into the hub. Leave
   the barrel jack unplugged.
2. Press and hold the **recovery button** — a pinhole on the underside, opposite the HDMI
   connector; use a paperclip — and, still holding it, **plug in the DC barrel jack**.
   Release when the screen changes.
3. The recovery screen appears. Press **Ctrl+D** on the USB keyboard. There is no
   on-screen prompt for it.
4. Confirm by **pressing the recovery button again** with the paperclip — on a device
   with no keyboard the firmware treats that button as Enter — then **Ctrl+D** to reboot.
5. It wipes and transitions to developer mode. Allow 10 to 15 minutes.

**Enabling external boot.** From a ChromeOS shell (Ctrl+Alt+T, then `shell`), once:

```sh
sudo crossystem dev_boot_usb=1 dev_boot_signed_only=0
```

Then reboot and press **Ctrl+U** at the "OS verification is OFF" screen. A 2.4 GHz
keyboard with its own USB receiver works as well as a wired one; a Bluetooth keyboard
does not, because the firmware has no Bluetooth stack.

If a boot fails, the board tells you by rebooting: the signed command line carries
`panic=30`, so a kernel panic or an initramfs that gives up on root returns to the
firmware splash about 30 seconds later. A board that *never* reboots means the kernel
never reached the initramfs at all — which on a machine with no serial console is the
single most useful thing a failed boot can say. A panic also writes a full dmesg to
`BOOT2DEB-PANIC.txt` on every ext4 partition it can reach.

Expect several seconds of blank HDMI on a healthy boot before the console appears: the
standard image leaves the DRM stack out of the initramfs to keep the signed payload under
its 16 MiB ceiling, so the display comes up only once the real root is mounted.

## Installing to the eMMC

Running from a USB 2.0 stick works but is slow, and it means the hub stays plugged in
forever. The board has 16 GB of eMMC, and once the OS is on it the Chromebit boots with
no keyboard, no stick and no hub — Ctrl+D at the developer screen, or just wait out the
30-second timeout.

The image is a whole-disk image, so installing it is one command. Boot the USB stick, join
Wi-Fi, get the same `.img.xz` onto the running system (`scp` it from the build host, or
keep a copy on the stick — first boot grows the rootfs, so there is room), and write it to
the internal eMMC:

```sh
lsblk                       # the eMMC is mmcblk0 — it is the one with mmcblk0boot0 beside it
xzcat asus-chromebit-cs10-forky.img.xz | sudo dd of=/dev/mmcblk0 bs=4M status=progress conv=fsync
sudo reboot                 # then Ctrl+D to boot the eMMC, or wait out the timeout
```

First boot on the eMMC then does what it did on the stick: it grows the rootfs to fill the
device, gives it fresh UUIDs, and re-signs the kernel against them into both of that
medium's kernel slots. The stick can stay plugged in — the two installs do not collide,
because first boot already gave the stick its own UUIDs, and Ctrl+D and Ctrl+U pick
between the two *media* explicitly, each of which carries its own pair of slots.

**Why this needs no kernel patch, contrary to the usual advice.** The Veyron eMMC ships
with its primary GPT deliberately corrupted — ChromeOS marks it `IGNOREME` and uses the
secondary — and a stock Linux kernel cannot read a partition table like that. Every guide
therefore tells you an eMMC install needs a patched kernel. That is true only if you
*keep* the factory GPT. Writing a whole-disk image does not: it lays down a fresh, valid
GPT over the top, and a stock kernel reads that one like any other. (postmarketOS does
exactly this on the C201's eMMC and boots from it.)

## Keyboard

A board with no keyboard still declares a console keymap, and it is not a contradiction.
The question `keymap` answers is "does a console layout configure anything here?" — not
"does the board have keys". The Chromebit drives an HDMI console that a USB keyboard is
the only way to type at, so a layout means exactly what it means on a laptop; it just
describes a keyboard you bring. The default is `us`.

```sh
cargo run -p boot2deb-cli -- build asus-chromebit-cs10-forky --keymap gb
sudo dpkg-reconfigure keyboard-configuration && sudo setupcon   # or, on the board
```

This has no bearing on the firmware screens. Depthcharge reads Ctrl+U with its own USB
HID driver and its own fixed layout, long before Linux exists.

See [Locale, timezone, and keyboard](../localization.md).

## Getting online

There is no ethernet port, so Wi-Fi is the only way onto the network:

```sh
sudo nmtui        # pick "Activate a connection", choose the network, enter the key
```

The radio is the family's Broadcom BCM4354 and needs two blobs Debian does not ship; they
are vendored on the SoC layer and are already in the image. Bluetooth works the same way
as on the laptops — the BCM4354's Bluetooth half is on `uart0`, the kernel loads the
vendored patchram, and `bluez` is installed to use it.

## Audio and display

Both come out of the HDMI connector and nothing else does.

**Audio is HDMI only.** The Chromebit's sound node wires straight to the HDMI codec with
no `audio-codec` phandle, so the machine driver builds a different card entirely: ALSA
shows `ROCKCHIP-HDMI`, not the `ROCKCHIP-MAX98090-HDMI` the laptops get. There is no
max98090, no headset codec, and nothing to unmute — the family's `20-audio` first-boot
hook probes for the codec's mixer controls, does not find them, and exits without
touching anything. That is the correct outcome, not a failure.

**Display is HDMI only, and simpler than on the laptops.** The Chromebit lights one
display controller (`vopb`); there is no eDP, no panel and no backlight. That also means
it is free of the trap the C201 has, where two controllers advertise the same maximum and
DRM picks between them at runtime.

HDMI does **4K30** and cannot do 4K60. That is the silicon: the RK3288 caps TMDS at
340 MHz, its PHY has no scrambling above that, and the VOP cannot emit YUV420, so there is
no reduced-rate path either. Nothing in the image configures any of this.

## Status

**Not yet booted on hardware.** The image builds, and everything it is made of is
shared with a board that does boot: the same boot method, the same signed-payload flow,
the same initramfs, the same radio and the same Debian kernel as the C201, which is
confirmed booting to a login prompt. What is untested is this board's own firmware, and
its DTB.

Two things to know before you try it, because they are the reported failure modes:

- **USB boot on the Chromebit has been reported to fail** — a 2015 review could not boot
  a stick at all, and there is an unanswered forum thread where Ctrl+U flashes black on a
  postmarketOS image. Both are most consistent with malformed boot media (a kernel
  partition that is not in ChromeOS format, or one whose GPT attribute bits are unset)
  rather than a firmware limit; boot2deb's payload is built by `depthchargectl` against
  the board's own profile and its attribute bits are asserted at build time. Expect it to
  work, but this is the board's open question.
- **No one has confirmed an eMMC install on a Chromebit.** The one public attempt fails
  with "Primary GPT header is being ignored", which is the factory GPT being preserved —
  the thing writing a whole-disk image does not do. The reasoning in *Installing to the
  eMMC* above is sound and the same flow works on the C201, but it has not been run on
  this board.

Audio, Bluetooth and HDMI ship configured and are unverified here.

## The family

The Chromebit is the awkward member of the family and it still costs exactly one file. It
ships no overlay and needed no change to the engine — everything that makes a Veyron boot
lives on the SoC and boot-method layers, and a board that is a stick rather than a laptop
inherits all of it unchanged. The same holds for the [C100P](asus-c100p.md), and for the
seven Veyron boards not yet written.
