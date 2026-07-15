# Turing RK1

The [Turing RK1](https://turingpi.com/product/turing-rk1/) is an RK3588 compute
module that seats in a Turing Pi 2 cluster board. boot2deb ships it as a small family
of recipes over one validated hardware base — kernel `v7.1.1` (linux-stable), u-boot
`v2026.04`, and the RGA / VEPU / VDPU (and NPU) drivers carried in-kernel via the
`rk3588-accel` patch profile. It is a supported configuration in its own right and a
good starting point for any RK3588 board.

The variants differ along two independent axes — the Debian suite, and whether the
Rockchip media **userspace** is built in:

| Recipe | Suite | Media userspace |
| --- | --- | --- |
| `turing-rk1-forky` | forky | — (base) |
| `turing-rk1-trixie` | trixie | — (base) |
| `turing-rk1-media-accel-forky` | forky | ffmpeg-rk + MPP + RGA |
| `turing-rk1-media-accel-trixie` | trixie | ffmpeg-rk + MPP + RGA |

Every variant carries the **same accel kernel**: the VEPU / VDPU / RGA and NPU drivers
are present in all of them, because the patches and kconfig live on the kernel axis. A
**base** image simply omits the Rockchip media userspace — the hardware blocks are
there but dark. A **media-accel** image adds the `media-accel-rockchip` feature, which
builds and installs `ffmpeg-rk`, `librockchip-mpp1`, and `librga2` on top. The split is
deliberate: because the kernel already carries the capability, those debs can equally be
installed onto a running base image later. `forky` is the RK1's validated suite.

Build the base image as in [Getting started](../getting-started.md):

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky
```

or, for a ready hardware-transcode host, the media-accel variant:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-media-accel-forky
```

Either produces `build/<recipe>/artifacts/turing-rk1.img.xz` — a whole-disk image (GPT,
u-boot in the reserved gap ahead of the first partition, then the ext4 rootfs), so a
single write lays down everything, bootloader included. The flashing and boot notes
below use `turing-rk1-forky`; they are identical for any variant (the bootloader and
disk layout do not change), so substitute your recipe name in the artifact path.

## Flash

The RK1 is a compute module, not a board you plug a card reader into, so the usual
path is the Turing Pi's BMC, which writes the module's **eMMC**:

- **`tpi flash -n <node> -l -i /absolute/path/to/turing-rk1.img`** — copy the image to
  the BMC first (e.g. onto its SD card, mounted at `/mnt/sdcard`) and use an absolute
  path, or
- the **BMC web UI**'s flash upload.

Both write eMMC only. For a removable or NVMe/USB medium you write on another machine,
decompress and `dd` it — the same image boots from any medium the board scans, since
u-boot discovers its root device at runtime:

```sh
xzcat build/turing-rk1-forky/artifacts/turing-rk1.img.xz \
  | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync   # confirm /dev/sdX with lsblk
```

The `tpi` CLI and web UI evolve; see Turing Pi's
[flashing docs](https://docs.turingpi.com/docs/turing-rk1-flashing-os) for the current
specifics.

## u-boot on eMMC, OS on a separate disk

A common RK1 setup keeps only u-boot on the eMMC and runs the OS from an NVMe or USB
disk. The builder produces the two pieces for this directly.

**The whole split at once** — build the `split` layout, which emits two images instead
of one:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky --layout split
```

- `turing-rk1-boot.img` — u-boot only (idbloader + `u-boot.itb` at their offsets, no
  GPT), for the eMMC.
- `turing-rk1-rootfs.img` — GPT + rootfs, for the NVMe/USB disk.

**Just the bootloader** — if you only need the eMMC u-boot image (e.g. to re-flash the
bootloader across nodes) without building a whole OS, the u-boot stage emits it on its
own:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky --stage uboot
```

This writes `turing-rk1-boot.img` (a few MiB, gap-sized) alongside the raw `idbloader.img`
and `u-boot.itb`. Flash `turing-rk1-boot.img` to the eMMC with `tpi`/web UI; write the
rootfs image to the target disk.

Because `tpi`/web UI flash the eMMC only, the rootfs image goes onto the NVMe/USB disk
by another route — typically `dd` from a running system on the node, or written on
another machine.

## Serial console

To watch u-boot and the kernel come up, open the node's UART from the BMC:

```sh
tpi uart --node <n> get
# or, on the BMC directly:
picocom /dev/ttyS<n> -b 115200
```

On BMC firmware **2.1.0 and newer** the node number maps 1:1 to the `ttyS` number
(node 1 → `ttyS1`, node 2 → `ttyS2`, …). On **2.0.5 and older** the mapping was offset
(node 1 → `ttyS2`, node 2 → `ttyS1`, …), so check your firmware version. The baud rate
is 115200. See Turing Pi's [UART docs](https://docs.turingpi.com/docs/tpi-uart).

## First boot

Power the node on. On first boot the image:

- **regenerates its SSH host keys**, and
- **grows the rootfs** to fill the whole medium (the 2 GB image expands to the disk's
  capacity). This reboots the node once to pick up the resized partition, so the first
  power-on comes up, reboots itself, then settles.

Log in as user **`debian`** with the password the build printed. It is expired, so you
are required to set a new one immediately. The `debian` account has passwordless
`sudo`, and the hostname is `turing-rk1`.

That is a booted Debian system. The kernel's transcode devices come up on **every**
variant — check for `/dev/dri` and `/dev/rga`. A **media-accel** image also installs the
`ffmpeg-rk` userspace, so you can exercise the `rkmpp` / `rkrga` paths directly; on a base
image the blocks are present but idle until you install the media-accel debs (or build a
`turing-rk1-media-accel-*` image).
