# Turing RK1

The [Turing RK1](https://turingpi.com/product/turing-rk1/) is an RK3588 compute
module that seats in a Turing Pi 2 cluster board. boot2deb ships it as a small family
of recipes over one hardware base — kernel `v7.1.6` (linux-stable), u-boot
`v2026.07`, and the RGA / VEPU / VDPU (and NPU) drivers carried in-kernel via the
`rk3588-accel` patch series. It is a supported configuration in its own right and a
good starting point for any RK3588 board.

The variants differ along two independent axes — the Debian suite, and whether the
Rockchip media **userspace** is built in:

| Recipe | Suite | Media userspace |
| --- | --- | --- |
| `turing-rk1/forky` | forky | — (base) |
| `turing-rk1/trixie` | trixie | — (base) |
| `turing-rk1/media-accel-forky` | forky | ffmpeg-rk + MPP + RGA |
| `turing-rk1/media-accel-trixie` | trixie | ffmpeg-rk + MPP + RGA |
| `turing-rk1/jellyfin-forky` | forky | ffmpeg-rk + MPP + RGA, plus Jellyfin |
| `turing-rk1/jellyfin-trixie` | trixie | ffmpeg-rk + MPP + RGA, plus Jellyfin |

The `jellyfin-*` pair is the media-server build — media-accel plus the Jellyfin
server, pre-pointed at `ffmpeg-rk`; see
[Accelerated Jellyfin](../jellyfin.md). `turing-rk1/util` is not an image along
these axes at all but a u-boot-only recovery tool — see
[Writing the NVMe from u-boot](#writing-the-nvme-from-u-boot).

Every variant carries the **same accel kernel**: the VEPU / VDPU / RGA and NPU drivers
are present in all of them, because the patches and kconfig live on the kernel axis. A
**base** image simply omits the Rockchip media userspace — the hardware blocks are
there but dark. A **media-accel** image adds the `media-accel-rockchip` feature, which
builds and installs `ffmpeg-rk`, `librockchip-mpp1`, and `librga2` on top. The split is
deliberate: because the kernel already carries the capability, those debs can equally be
installed onto a running base image later. `forky` is the RK1's validated suite.

Build the base image as in [Getting started](../getting-started.md):

```sh
boot2deb build turing-rk1/forky
```

or, for a ready hardware-transcode host, the media-accel variant:

```sh
boot2deb build turing-rk1/media-accel-forky
```

Either produces a whole-disk image (GPT, u-boot in the reserved gap ahead of the first
partition, then the ext4 rootfs), so a single write lays down everything, bootloader
included. Artifacts are named for the whole build point, so `turing-rk1/forky` writes
`build/turing-rk1/forky/artifacts/turing-rk1-forky.img.xz` and the media-accel variant
writes `turing-rk1-media-accel-forky.img.xz`. The flashing and boot notes below use
`turing-rk1/forky`; they are identical for any variant (the bootloader and disk layout
do not change), so substitute your recipe name throughout.

## Flash

The RK1 is a compute module, not a board you plug a card reader into, so the usual
path is the Turing Pi's BMC, which writes the module's **eMMC**:

- **`tpi flash -n <node> -l -i /absolute/path/to/turing-rk1-forky.img`** — copy the image to
  the BMC first (e.g. onto its SD card, mounted at `/mnt/sdcard`) and use an absolute
  path, or
- the **BMC web UI**'s flash upload.

Both write eMMC only. For a removable or NVMe/USB medium you write on another machine,
decompress and `dd` it — the same image boots from any medium the board scans, since
u-boot discovers its root device at runtime:

```sh
xzcat build/turing-rk1/forky/artifacts/turing-rk1-forky.img.xz \
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
boot2deb build turing-rk1/forky --layout split
```

- `turing-rk1-forky-boot.img` — u-boot only (idbloader + `u-boot.itb` at their
  offsets, no GPT), for the eMMC.
- `turing-rk1-forky-rootfs.img` — GPT + rootfs, for the NVMe/USB disk.

**Just the bootloader** — if you only need the eMMC u-boot image (e.g. to re-flash the
bootloader across nodes) without building a whole OS, the u-boot stage emits it on its
own:

```sh
boot2deb build turing-rk1/forky --stage uboot
```

This writes `turing-rk1-forky-boot.img` (a few MiB, gap-sized) alongside the raw
`turing-rk1-forky-idbloader.img` and `turing-rk1-forky-u-boot.itb`. Flash the
`-boot.img` to the eMMC with `tpi`/web UI; write the rootfs image to the target disk.

Because `tpi`/web UI flash the eMMC only, the rootfs image goes onto the NVMe/USB disk
by another route. The bootloader itself is the shortest one — see below.

## Writing the NVMe from u-boot

The BMC writes eMMC and nothing else: the loader it streams into the module speaks
eMMC, so the M.2 disk is invisible to `tpi flash`, to the web UI, and to gadget
mode. The RK1's own u-boot has no such limit — it enumerates the disk over
`pcie3x4` — so the shipped bootloader carries the two commands that let a host
reach it. Build the tool variant for the full set:

```sh
boot2deb build turing-rk1/util --stage uboot     # writes turing-rk1-util-boot.img
```

Flash that to the eMMC with `tpi`, open the node's UART, and interrupt the
countdown. Two routes from the prompt:

**Export the disk to the BMC.** `ums` presents any block device u-boot can see as
USB mass storage, so with the node's USB in device mode the BMC sees the NVMe as a
normal disk:

```
=> nvme scan
=> ums 0 nvme 0
```

then, from your machine, stream the image through the BMC — nothing is staged on
the node or the BMC:

```sh
xzcat turing-rk1-forky-rootfs.img.xz | ssh root@<bmc> 'dd of=/dev/sdX bs=4M'
```

**Or pull the image in over the network** and let u-boot write it. This needs a
gzip image, since u-boot has no xz decompressor:

```sh
boot2deb build turing-rk1/forky --layout split --compress gz
```

```
=> dhcp
=> tftp ${loadaddr} turing-rk1-forky-rootfs.img.gz
=> gzwrite nvme 0 ${loadaddr} ${filesize}
```

`gzwrite` decompresses and writes in one pass. Hash the image first with `md5sum`
if the link is one you do not trust — several GiB over TFTP has no integrity check
of its own. Images at or above 4 GiB uncompressed need `gzwrite`'s explicit
`outsize` argument; the shipped recipes are well under it.

Either way the eMMC still needs a bootloader afterwards. `boot2deb build
turing-rk1/forky --stage uboot` emits the shipping one, which also carries `ums` —
so a node that boots from NVMe keeps a route back to its disks without reflashing
the tool.

## Or keep the OS on eMMC and use the NVMe for data

Often the better answer, and it makes the whole errand above unnecessary: flash the
**entire** system to the eMMC — which the BMC can do in one step — and let the M.2
disk hold data only. Reimaging then never touches the data, because the new image
finds the volume by label and adopts it.

The RK1's 29 GB eMMC has room for any of the shipped images several times over, so
nothing is given up by keeping the OS there. No shipped recipe assumes this layout —
where the data lives is an installation's choice, not the board's — so you add it to
your own recipe. See [Data volumes](../data-volumes.md).

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
  capacity), online, in the same boot — no reboot involved.

Log in as user **`debian`** with the password the build printed. It is expired, so you
are required to set a new one immediately. The `debian` account has passwordless
`sudo`, and the hostname is `turing-rk1`.

That is a booted Debian system. The kernel's transcode devices come up on **every**
variant — check for `/dev/dri` and `/dev/rga`. A **media-accel** image also installs the
`ffmpeg-rk` userspace, so you can exercise the `rkmpp` / `rkrga` paths directly; on a base
image the blocks are present but idle until you install the media-accel debs (or build a
`turing-rk1/media-accel-*` image).

### Running the accelerated FFmpeg

`ffmpeg-rk` installs under `/opt/ffmpeg-rk` and is on `PATH` as **`ffmpeg-rk`** (and
`ffprobe-rk`). The suffix is deliberate: the build ships the same library sonames as
Debian's own FFmpeg, so it is kept out of the system's paths and out of the loader's
search path, and the plain `ffmpeg` name stays with the distro package.

```sh
ffmpeg-rk -hide_banner -filters  | grep rkrga      # scale_rkrga, vpp_rkrga, overlay_rkrga
ffmpeg-rk -hide_banner -encoders | grep rkmpp      # h264_rkmpp, hevc_rkmpp
```

Hardware **decode** is reached with `-hwaccel v4l2request`, not with the `*_rkmpp`
decoders — those are compiled in but do not open on a mainline kernel, where `rkvdec`
is a V4L2 stateless driver rather than an MPP service. A transcode that scales looks
like this, and scales on the CPU:

```sh
ffmpeg-rk -hwaccel v4l2request -i in.mkv \
          -vf "hwdownload,format=nv12,scale=1280:720" \
          -c:v hevc_rkmpp out.mp4
```

Both of those limits are stated as caveats on the `media-accel-rockchip` feature, so
they print at the end of a build of any recipe composing it; see
[Support matrix](../reference/support-matrix.md#caveats).
