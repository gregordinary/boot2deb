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
cargo run -p boot2deb-cli -- build h96-max-m9/forky
```

That writes `build/h96-max-m9/forky/artifacts/h96-max-m9.img.xz` — GPT, u-boot in the
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
xzcat build/h96-max-m9/forky/artifacts/h96-max-m9.img.xz \
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
| HDMI-CEC | works — the box wakes, switches to and standbys a TV. Driving the box from the TV remote needs a kernel with `MEDIA_CEC_RC` |
| NPU (`rocket`) | device works — jobs compute bit-exact; no userspace for this SoC yet |
| HDMI audio | works |
| S/PDIF (optical) | works |
| Analog audio (3.5 mm) | root-caused and fixed in tree; awaiting a build to confirm end to end |
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
  The `rk3576-fixes` series carries the driver fix. Verified at the register level by
  driving each SDO port in turn and listening; an image build confirms it end to end.

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
