# H96 MAX M9

The H96 MAX M9 (and the M9S, the same board) is an Android TV box built on the
Rockchip **RK3576** — octa-core (4x Cortex-A72 + 4x Cortex-A53), Mali-G52 MC3,
LPDDR4X, eMMC 5.1, Gigabit ethernet, and HDMI. boot2deb turns it into a mainline
Debian box: kernel `v7.1.5`, u-boot `v2026.04`, no vendor BSP.

It is a cheap and widely available RK3576 board, which makes it a practical target —
and an awkward one. There is no SD slot (the pads are depopulated), no reset button,
and no exposed serial header until you open it, so most of the work of supporting it
is having a bootloader that can recover the board without a cable. That is what the
[RK3576 u-boot images](../reference/rk3576-uboot-images.md) exist for.

## Recipes

| Recipe | Deliverable | Status |
| --- | --- | --- |
| `h96-max-m9/forky` | Whole-disk Debian image (forky) | expected — the board has booted this configuration, but not at this pin |
| `h96-max-m9/media-accel` | The same image plus HW video decode and the RGA 2D accelerator | experimental |
| `h96-max-m9/util` | u-boot only — the recovery tool, with this board's ethernet | builds; ethernet validated |

The base image carries the NPU — see [The NPU](#the-npu) below.

Build the shipped image as in [Getting started](../getting-started.md):

```sh
boot2deb build h96-max-m9/forky
```

That writes `build/h96-max-m9/forky/artifacts/h96-max-m9-forky.img.xz` — GPT, u-boot in the
raw gap ahead of the first partition, then the ext4 rootfs, so one write lays down
everything.

## Flash

The box boots from eMMC and has no card slot, so every path goes over USB through the
**USB 3.0 Type-A port on the rear panel** — that connector is the SoC's `drd0`
controller, wired as a USB *device*. A plain USB-A-to-USB-A cable to your laptop is
what talks to it.

Two routes, in order of preference:

**1. From a running u-boot (`ums`), no vendor tooling.** Stream the util image into
RAM over maskrom (see [The maskrom loader](../reference/maskrom-loader.md)), then at
the u-boot prompt:

```
ums 0 mmc 0
```

The eMMC appears on your laptop as a USB block device. Write the image to it as you
would any disk:

```sh
xzcat build/h96-max-m9/forky/artifacts/h96-max-m9-forky.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync   # confirm /dev/sdX with lsblk
```

**2. Over rockusb**, with `rkdeveloptool` and Rockchip's `RK3576_MiniLoaderAll.bin`.
This is the route community documentation describes; it works, and it is the fallback
if you have no u-boot on the board yet and prefer the vendor path to streaming ours.

Reading the eMMC back over rockusb does **not** work — the read path truncates at
32 MiB. Use `ums` (above) or `dd` from a booted system.

### Getting into maskrom

Maskrom is the BootROM's USB download mode, and it is the entry that depends on
nothing already working on the board.

- **The floor, on any firmware:** short the two **eMMC test pads** (clock and ground,
  on the solder side next to the `EMMC` silkscreen) at power-on. The BootROM cannot
  clock data out of the flash and falls through to USB device mode. The status LED
  glowing *dim* rather than bright confirms it.
- **Once the board runs boot2deb's u-boot:** press the recessed button in the AV jack
  before connecting power. Our u-boot carries the download-key patches, so the button
  drops straight into maskrom.

The AV-jack button behaves differently on factory firmware depending on the build —
some reach loader mode, the reference unit's newer firmware boots Android recovery
instead — which is exactly why the pad short is the documented floor.

## Serial console

The UART is on an unpopulated 3-pin header inside the case, and it runs at
**1500000 baud**, not 115200 — a Rockchip default that will otherwise look like a dead
port.

You do not need it. The `display` u-boot the image ships with drives the **HDMI console
and a USB keyboard** on the drd1 (USB 2.0) port, so the u-boot prompt, the boot menu,
and a rescue-stick boot are all reachable on the television.

## First boot

Power on. The image regenerates its SSH host keys and grows the rootfs to fill the
eMMC, online, in the same boot. Log in as **`debian`** with the password the build
printed; it is expired, so you set a new one immediately. The account has passwordless
`sudo` and the hostname is `h96-max-m9`.

## Hardware status

Validated on the reference unit (8 GB / 128 GB) running a boot2deb image:

| Subsystem | State |
| --- | --- |
| Boot to HDMI login, eMMC | works |
| Ethernet (GMAC0) | works |
| eMMC (HS400-ES) | works |
| HDMI video | works — up to 340 MHz TMDS, so 1080p60, 1440p60 and 4K30 |
| HDMI hotplug + EDID | works — unplug/replug re-reads EDID |
| GPU (Mali-G52 / panfrost) + Mesa GL | works — GL 3.1 / GLES 3.1, full desktop composites on it |
| Wi-Fi, 2.4 + 5 GHz | works (AIC8800D80) |
| Bluetooth | works — `hci0` up, LE + classic scan |
| Suspend / resume (s2idle) | works |
| USB 2.0 host | works |
| Bundled remote | works, zero-config |
| IR receiver | works — NEC decoded to input events |
| HDMI-CEC | works — the box wakes, switches to and standbys a TV; opt-in, see below |
| NPU (`rocket`) | device works — jobs compute bit-exact; no userspace for this SoC yet |
| HDMI audio | works |
| S/PDIF (optical) | works |
| Analog audio (3.5 mm) | fixed in tree — the DAC is on `sdo2`; end-to-end confirmation on a shipped image still owed |
| HW video decode | works — 1080p H.264 and HEVC on the VDPU383, NV12 out |
| HW video encode | no mainline driver |
| SD card | absent — the slot is depopulated |
| USB 3.0 SuperSpeed | not available on any port — see below |

Things the board needs that are worth knowing about:

- **Wi-Fi is an out-of-tree module.** The AIC8800D80 has no mainline driver, so
  boot2deb builds one from a pinned upstream repo as a `.deb` through the
  [kmods layer](../reference/config-model.md#out-of-tree-modules-are-their-own-layer)
  — declared by the device as `device_kmods = ["aic8800"]`, not carried as a kernel
  patch series, so an RK3576 board without the chip gets a lean kernel.
- **`cpuidle.off=1` is in the kernel command line.** A core suspended into the DT
  `CPU_SLEEP` state can miss its wakeup on this platform's BL31. It is a board-level
  workaround, stated in `devices/h96-max-m9.toml` with the condition to drop it.
- **No port on the box delivers USB 3.0**, for two unrelated reasons. The blue port
  beside HDMI is `drd0`, capped to high speed in the board `.dts` because SuperSpeed
  training collapses into a `-62/-71` SetAddress loop that takes the boot medium with
  it. The black ports are `drd1`, and they sit behind an internal `1a86:8091` 4-port
  **USB 2.0** hub that also carries the bundled remote's receiver. `drd1` does register
  a SuperSpeed root hub, but its lane reaches no connector, so that bus is always empty.
- **4K60 is not reachable on any `dw-hdmi-qp` board**, this one included. The bridge
  rejects every mode above 340 MHz TMDS because it has no SCDC/scrambling support, so
  4K30 (297 MHz) is the ceiling even when the display advertises 4K60. This is upstream
  behaviour, not a board or device-tree limitation.
- **The 3.5 mm analog jack needed both a device-tree and a kernel fix.** Its DAC sits on
  SAI1's `sdo2`, and reaching it takes `rockchip,sai-tx-route = <0 1 0>` — in
  `SAI_PATH_SEL`, TX field *x* selects which stream drives **SDO port x**, so the third
  entry is the one that matters. The upstream SAI driver programmed that register only
  at probe and the value did not survive to the running device, which no in-tree board
  could notice because they all describe the identity mapping the register already holds.
  The `rk3576-fixes` series carries the driver fix, as patch 103. Verified at the register
  level by driving each SDO port in turn and listening; the device tree alone cannot fix
  this board.

## HDMI-CEC

CEC is a control channel inside the HDMI cable, and this box drives it both ways: it can
wake a television, claim its input and put it back into standby, and the television's own
remote can drive the box. The kernel side is present in every image for this board —
`DRM_DW_HDMI_QP_CEC` and `MEDIA_CEC_RC` are set at the SoC layer — so `/dev/cec0` exists on
a fresh boot and the adapter comes up as Playback Device 1 at physical address `1.0.0.0`.

None of it acts until you ask. Three units ship installed but **disabled**, because a box
plugged into a computer monitor, or into a television whose owner would rather it kept to
itself, must not start sending CEC messages on its own:

| Unit | What it sends | When |
| --- | --- | --- |
| `cec-tv-on` | `IMAGE_VIEW_ON`, then `ACTIVE_SOURCE` — wakes the TV and switches it to this input | at boot, and on resume from sleep |
| `cec-tv-standby` | a directed `STANDBY` to the TV | at shutdown and reboot |
| `cec-passthrough` | nothing; runs `cec-follower` so the TV remote's keys arrive as input events | continuously |

Box drives TV, which is what most people want:

```sh
sudo systemctl enable --now cec-tv-on.service
sudo systemctl enable cec-tv-standby.service
```

`--now` on the first one wakes the TV immediately, which doubles as the test that it works.
The second deliberately has no `--now`: its work runs at *stop*, so it fires when the box
goes down, not when you enable it.

**Enabling `cec-tv-on` is also what makes the TV follow the box into sleep.** The
`h96-cec` hook in `/usr/lib/systemd/system-sleep/` reads that one unit for both directions
— with it enabled, a POWER press standbys the TV on the way down and wakes it on the way
back up. `cec-tv-standby` covers only shutdown and reboot, so enabling that one on its own
leaves suspend untouched: the box sleeps, the HDMI signal simply stops, and the television
shows its own "No Signal" instead of going dark. Without CEC that is the whole story of
what a suspend looks like on screen — the TV was never in standby, it just lost its source.

For the other direction, the TV's remote driving the box, enable `cec-passthrough` **and**
`cec-tv-on`. The dependency is not incidental: a television forwards `USER_CONTROL_PRESSED`
only to whatever it believes is the active source, so a box that never announced itself
receives nothing.

Once `cec-tv-on` has run the adapter stays configured, so the bus is visible from the box:

```sh
cec-ctl -d /dev/cec0 -S
```

A television that implements CEC answers as `0.0.0.0: TV` with its vendor ID. Everything
else on the cable is listed too, which is worth reading before enabling `cec-tv-standby`:
the standby it sends is directed at the TV rather than broadcast, precisely so a games
console or receiver sharing the bus is not put to sleep along with it.

Two results that look like faults and are not:

- **A physical address is not evidence of CEC support.** `Physical Address: 1.0.0.0` is
  derived from a mandatory EDID field, so it is reported even when nothing at the other end
  speaks the protocol. The real test is whether messages are acknowledged;
  `Tx, Not Acknowledged (4), Max Retries` means nothing is driving the CEC line at all.
  Computer monitors generally do not, including ones from vendors whose televisions do.
- **Some queries go unanswered.** `GIVE_OSD_NAME` and `GIVE_CEC_VERSION` time out against
  televisions that omit them, and a CEC 2.0 `REPORT_FEATURES` can come back
  `unrecognized-op`. Those are the sink's omissions — the transmits themselves are
  acknowledged, so nothing local is wrong.

The box appears in the TV's device list as `H96 MAX M9`. The wrappers in `/usr/lib/h96/`
take `OSD_NAME` and `CEC_DEV` from the environment if a different name or a second adapter
is wanted; CEC caps the name at 14 characters.

## Sleep and the POWER key

The remote's POWER key suspends the box, and the same key wakes it. That is the shipped
default rather than systemd's, and the reason is that the alternative is not recoverable
here: the box has no power button, and once it is off the remote's receiver is unpowered,
so `HandlePowerKey=poweroff` would leave cycling the supply as the only way back. To
choose `poweroff` or `ignore` anyway, see `/usr/lib/h96/power-profiles/README` on the
board.

Sleep is **suspend-to-idle**, pinned by `50-h96-s2idle.conf`. The SoC also offers `deep`,
and selects it by default, but nothing resumes from it — the box powers down and needs
its supply cycled — so the image names `freeze` explicitly rather than letting systemd
write `mem`. Anything that asks logind to suspend, a desktop's idle timer included, takes
that path.

The television does not follow the box unless you tell it to. Suspending stops the HDMI
signal and nothing more, which a TV shows as "No Signal" while staying on; putting it into
standby alongside the box is one `systemctl enable cec-tv-on.service` away, and
[HDMI-CEC](#hdmi-cec) covers what that turns on.

Waking is out of band: the receiver drives gpio0 PD3, the `gpio-keys` POWER input, which
is a `wakeup-source` in the always-on power domain. **The bundled remote is the only
wake source the box has out of the box.** Its receiver cannot wake anything over USB —
it leaves the remote-wakeup bit clear in its configuration descriptor, so the kernel
creates no `power/wakeup` node for it — and there is no RTC on this board, so there is no
`rtcwake` either.

A USB keyboard or mouse can wake it if its receiver *does* set that bit.
`70-h96-usb-wakeup.rules` arms every HID device that has a `power/wakeup` node, which
skips the bundled receiver and catches a wakeup-capable one. What is armed on a running
box:

```sh
grep . /sys/bus/usb/devices/*/power/wakeup
```

## The NPU

The RK3576 carries an RKNN neural accelerator, and mainline drives it — through the
in-tree `rocket` DRM-accel driver, no vendor RKNPU2 stack. **Every image for this board
has it**: the shipped `h96-max-m9/forky` binds `rocket` on `27700000.npu` and presents
**`/dev/accel/accel0`**.

Jobs submitted to it compute correctly: an int8 convolution is bit-exact against a CPU
model on this silicon, and multi-task row-windowed programs stay bit-exact submitted
back to back with no gap.

The image supplies the *device*, not a runtime, and **there is no released userspace for
this SoC yet**. Nothing in Debian opens an accel node at all, and the NPU's register
program is SoC-specific — the RK3576 map is shifted and re-packed relative to the RK3588,
so a userspace written for the RK3588 does not run here.
[rocket-userspace](https://github.com/gregordinary/rocket-userspace) is the library to
watch: it is bit-exact on the RK3588 today and names the RK3576 as its next target, but
its machine parameters (CBUF size, core count, datatype set) have to be confirmed on this
part before it drives this board. Until then, driving the NPU here means writing your own
regcmd encoder against `/dev/accel`.

Two properties of the board's DTS are load-bearing and fail in ways that do not point
at themselves — both power domains (`NPU0` *and* `NPU1`) on the one core node, or the
driver's own domain attach loses to the device core's and there is no `/dev/accel`; and
`regulator-always-on` on `vdd_npu_s0`, or the rail is torn down as unused ~33 s into
boot, long after a successful probe. The board `.dts` states both with the reasoning.

That always-on rail is the standing cost of carrying the NPU in the base image: it holds
a supply up on a board that may never open the accel node.

## Related pages

- [RK3576 u-boot images](../reference/rk3576-uboot-images.md) — the loader / display /
  util split, what each can do, and why the u-boot variant is its own axis.
- [The maskrom loader](../reference/maskrom-loader.md) — streaming a u-boot into RAM
  over the BootROM download protocol.
- [Support matrix](../reference/support-matrix.md) — every recipe with the exact pins
  its lock records.
