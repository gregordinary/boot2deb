# Turing RK1

The [Turing RK1](https://turingpi.com/product/turing-rk1/) is an RK3588 compute
module that seats in a Turing Pi 2 cluster board. boot2deb ships it as a small family
of recipes over one hardware base — kernel `v7.2` (linux-stable), u-boot
`v2026.07`, and the RGA / VEPU / VDPU (and NPU) drivers carried in-kernel via the
`rk3588-accel` patch series. It is a supported configuration in its own right and a
good starting point for any RK3588 board.

The variants differ along two independent axes — the Debian suite, and whether the
Rockchip media **userspace** is built in:

| Recipe | Suite | Media userspace |
| --- | --- | --- |
| `turing-rk1/forky` | forky | — (base) |
| `turing-rk1/trixie` | trixie | — (base) |
| `turing-rk1/media-accel-forky` | forky | ffmpeg-rk + MPP + RGA + Vulkan |
| `turing-rk1/media-accel-trixie` | trixie | ffmpeg-rk + MPP + RGA + Vulkan |
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

### Two RGA drivers exist; this image builds the out-of-tree one

Kernel 7.2 added an in-tree V4L2 driver for the RK3588's RGA3 cores, so from that
release the SoC has two drivers to choose between, and the choice is a kconfig one —
`ROCKCHIP_MULTI_RGA` for the vendor driver, `VIDEO_ROCKCHIP_RGA` for the in-tree one.
These images build the vendor driver and leave the in-tree one unset, for reasons that
are about what reaches FFmpeg rather than about code quality:

- **The ABI.** `librga`, and therefore `scale_rkrga`, `vpp_rkrga` and `overlay_rkrga`,
  speak the vendor `/dev/rga` interface. The in-tree driver exposes V4L2 video nodes,
  and FFmpeg ships no V4L2 mem2mem *filter* to drive them.
- **The cores.** The in-tree driver deliberately exposes one core, to avoid an ABI
  break when multi-core scheduling is added later. The vendor driver schedules across
  all three.
- **10-bit.** The in-tree driver implements scaling and colour conversion only; 10-bit
  YUV is on its own list of what is not done yet. A 10-bit decode on this hardware
  produces NV15, and RGA is the only block that converts it without a trip through
  system memory — see [10-bit, and why `NV15` is the whole
  story](#10-bit-and-why-nv15-is-the-whole-story).

If you want a kernel with no out-of-tree code and can live without the FFmpeg filter
path, the in-tree driver is a supported thing to switch to: unset
`CONFIG_ROCKCHIP_MULTI_RGA` and set `CONFIG_VIDEO_ROCKCHIP_RGA` in an overlay fragment,
and drop `media-accel/kernel/072` from the series so the device-tree nodes keep their
mainline compatibles.

The `media-accel-*` pair also carries the **`vulkan`** feature: Mesa's Vulkan drivers
and the loader, which is what makes the Vulkan filters `ffmpeg-rk` is already built
with actually open. On a box driven from the command line those are the fastest scale,
tone-map and composite route the hardware has — `scale_vulkan` measures 53.8 dB against
swscale at 18x the efficiency and 2.8x the speed. It costs about 305 MiB installed, two
thirds of which is the LLVM that Mesa's software rasterizer pulls in rather than the
Mali driver itself. The `jellyfin-*` pair deliberately does **not** carry it: Jellyfin's
`HardwareAccelerationType` is a closed enum with no Vulkan member, so the server can
never emit a Vulkan filter and the packages would be reachable only from a shell. Add
it there with `turing-rk1/forky+media-accel-rockchip+jellyfin+jellyfin-rockchip+vulkan`
if you want the command line too — with `--image-size 3G`, since a feature selection
takes the device's 2G and cannot carry the `jellyfin-*` recipes' own larger volume.

Every image here ships a **redistributable** FFmpeg; see
[The FFmpeg a build ships is redistributable](../reference/config-model.md#the-ffmpeg-a-build-ships-is-redistributable)
for the opt-in nonfree flavour and why no hardware path depends on it.

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

Press the built artifact into a verified, optionally personalized raw image
first — one master, one file per unit:

```sh
boot2deb press turing-rk1/forky rk1-03.img \
    --hostname rk1-03 --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
```

The RK1 is a compute module, not a board you plug a card reader into, so the
usual write path is the Turing Pi's BMC, which writes the module's **eMMC**.
Both BMC routes take the raw file `press` produces:

```sh
tpi flash -n 2 -l -i rk1-03.img       # the tpi CLI, node 1-4
```

or the **BMC web UI**'s flash upload. For a removable or NVMe/USB medium you
write on another machine — the same image boots from any medium the board
scans, since u-boot discovers its root device at runtime — use any flasher,
`dd` included:

```sh
lsblk    # confirm the device; dd overwrites it whole
sudo dd if=rk1-03.img of=/dev/sdX bs=4M status=progress conv=fsync
```

See [Producing images](../press.md) for verification, the seed keys, and
per-site additions. The `tpi` CLI and web UI evolve; see Turing Pi's
[flashing docs](https://docs.turingpi.com/docs/turing-rk1-flashing-os) for the
current specifics.

### Streaming to the eMMC over USB mass storage

`tpi flash` stages the whole image on the BMC before it writes. The BMC has
another mode that does not:

```sh
tpi advanced msd --node 2
```

That reboots the node into USB mass-storage mode, after which its eMMC is an
ordinary SCSI disk **on the BMC** — no custom firmware, no u-boot in the loop,
no UART. Allow about ten seconds for it to enumerate, then find it by the
vendor string the RK1's eMMC reports rather than by guessing a letter:

```sh
ssh root@<bmc> 'grep -l Rockchip /sys/block/*/device/vendor'
# /sys/block/sda/device/vendor   ->  the node's eMMC is /dev/sda
```

With the disk present, the image streams straight through with nothing staged
on either machine:

```sh
xzcat rk1-03.img.xz | ssh root@<bmc> 'dd of=/dev/sda bs=4M conv=fsync'
```

`conv=sparse` is worth knowing about here and worth understanding before you
use it: it makes `dd` seek over runs of NULs instead of writing them, which is
a real saving across a USB mass-storage link, because most of a fresh image is
zeros. The catch is that seeking leaves whatever was there before — so it is a
faster write, not a clean one. On a node whose eMMC has held another system,
the stale bytes can include an old backup GPT or a filesystem signature that
`blkid` will still find. Use it on a disk you do not mind reading as
half-overwritten, and leave it off when you want the medium to say exactly what
the image says.

The same mode is the route by which an already-flashed node can be edited in
place rather than reflashed — mount the exposed rootfs on the BMC and change
what you need, which is how a `boot2deb seed` key can be applied after the
fact. Whether that half works is a property of the BMC firmware rather than of
this mode: it needs ext4 in the BMC's kernel and enough userland to be useful.
One command tells you before you plan around it:

```sh
ssh root@<bmc> 'grep -w ext4 /proc/filesystems && command -v mount'
```

The write half needs neither, so it stands on its own where the mount half does
not.

**This exposes the eMMC and nothing else**, exactly as `tpi flash` does. The
M.2 disk stays invisible to the BMC in this mode as in every other; see
[Writing the NVMe from u-boot](#writing-the-nvme-from-u-boot) and
[Installing to the NVMe from the booted node](#installing-to-the-nvme-from-the-booted-node)
for the two routes that reach it.

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

`press` emits the same pair as personalized copies —
`press turing-rk1/forky --layout split --boot-out emmc.img --rootfs-out
nvme.img --hostname rk1-03` — with the seed riding the rootfs half.

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

## Installing to the NVMe from the booted node

The shortest way onto the M.2 disk uses no serial console and no bootloader
prompt at all: flash the **eMMC** with the BMC — which it can do in one step —
carrying the image inside itself, boot that, and let the node write its own
NVMe.

```sh
boot2deb press turing-rk1/forky rk1-03.img --embed-image \
    --hostname rk1-03 --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
tpi flash -n 2 -l -i rk1-03.img
```

`--embed-image` carries the recipe's own compressed artifact inside the pressed
image at `/var/lib/boot2deb/install/`. Power the node on, let first boot finish,
then from an ssh session:

```sh
sudo boot2deb-install-to /dev/nvme0n1
```

`boot2deb-install-to` ships in every image. It refuses anything that is not a
whole disk, refuses the disk the system is running from, refuses a disk with
anything mounted on it, and requires you to type the device's name — then writes
the embedded artifact and syncs. Power off, and the node boots from the NVMe;
the rootfs grows to fill it on that first boot. Re-running it is safe and still
writes, so an interrupted write is repaired by repeating it.

That is one BMC flash and one ssh session. The eMMC keeps a complete, bootable
copy of the same system, which is a useful thing to have on a node whose OS
disk you are about to replace.

## Writing the NVMe from u-boot

The BMC writes eMMC and nothing else: the loader it streams into the module speaks
eMMC, so the M.2 disk is invisible to `tpi flash`, to the web UI, and to gadget
mode. The RK1's own u-boot has no such limit — it enumerates the disk over
`pcie3x4` — so the shipped bootloader carries the two commands that let a host
reach it.

This route needs a UART session and interrupting the boot countdown, so reach
for it when you want the disk written from *outside* the node — a bare M.2 with
no system on it yet, or a node whose OS will not boot. To install onto the NVMe
of a node that boots, [the previous section](#installing-to-the-nvme-from-the-booted-node)
does it with no console at all. Build the tool variant for the full set:

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

### Forcing one boot from a chosen medium

A node carrying a system on both its eMMC and its M.2 disk boots whichever its
`boot_targets` list reaches first. To boot the other one **once** — to check
that a freshly written eMMC copy comes up, say, without disturbing a node that
normally runs from NVMe — override the list at the prompt instead of writing it:

```sh
tpi uart --node 2 get          # watch this while the node powers on
tpi power on --node 2
```

Interrupt the countdown at `Hit any key to stop autoboot:` with any key, then:

```
=> printenv boot_targets        # the shipped order, whatever it is on your build
=> setenv boot_targets mmc0     # or nvme0, or "mmc0 nvme0" to try both
=> boot
```

`setenv` without `saveenv` lives until the next reset, so this cannot strand the
node: power-cycle it and the shipped order is back, unchanged. That is the whole
reason to prefer it over editing the environment.

Driving that conversation for you — powering the node and answering the prompt
in one command — is device tooling rather than an image builder's job, and
boot2deb does not do it; the same boundary that keeps it from writing devices.

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
is a V4L2 stateless driver rather than an MPP service.

A transcode that stays in hardware from decode through scale to encode names an RKMPP
device for the RGA filters to allocate from, which a `v4l2request` chain does not supply
on its own:

```sh
ffmpeg-rk -init_hw_device rkmpp=rk -filter_hw_device rk \
          -hwaccel v4l2request -hwaccel_output_format drm_prime -i in.mkv \
          -vf "scale_rkrga=w=1280:h=720:format=nv12" \
          -c:v hevc_rkmpp out.mp4
```

Without `-init_hw_device rkmpp=rk -filter_hw_device rk` the filter fails with `No RKMPP
hardware context provided`, which reads like a frame-format problem and is not one.

Both of those are stated as caveats on the `media-accel-rockchip` feature, so they print
at the end of a build of any recipe composing it; see
[Support matrix](../reference/support-matrix.md#caveats). What each block can actually
decode and encode is tabulated under [What the hardware decodes and
encodes](#what-the-hardware-decodes-and-encodes).

## What the hardware decodes and encodes

Three hardware blocks behind two decode drivers, plus one encoder. The table is what the
kernel this image builds actually offers; whether a given codec has been *validated* on
this board is a different question, answered by [`boot2deb
support-matrix`](../reference/support-matrix.md).

### Decode

| Codec | Block / node | 8-bit | 10-bit | Notes |
|---|---|---|---|---|
| HEVC | `rkvdec` VDPU381 | `NV12` | `NV15` | 4:2:0; two cores, one `/dev/video0` |
| H.264 | `rkvdec` VDPU381 | `NV12`, `NV16` | `NV15`, `NV20` | 4:2:0 and 4:2:2 |
| VP9 | `rkvdec` VDPU381 | `NV12` (profile 0) | `NV15` (profile 2) | profiles 1 and 3 are 4:2:2/4:4:4, which the hardware cannot do |
| AV1 | Hantro VPU981 | `NV12` | `NV15`, `P010` | to 8192x4352; decodes to a 4x4-tiled buffer and post-processes out |
| VP8 | Hantro VDPU2 | `NV12` | — | |
| MPEG-2 | Hantro VDPU2 | `NV12` | — | 1080p ceiling; progressive bit-exact, interlaced output diverges slightly |

Hantro's VDPU2 node also advertises H.264, at a 1080p ceiling and 8-bit only. `rkvdec`'s
is the one to use — it is the newer block, goes to 10-bit, and has two cores. Both score
identically on conformance (JVT-AVC_V1 129/135, the same six failures).

Conformance, measured on this board against the ITU/ISO suites with the software decoder
as control:

- **HEVC** (JCT-VC-HEVC_V1): **146/147**; the one failure is unequal luma/chroma bit
  depth, which software fails too. A stream that keeps its reference picture sets in the
  SPS needs the `EXT_SPS_ST_RPS`/`EXT_SPS_LT_RPS` controls filled — `ffmpeg-rk` carries
  the patch that does (`media-accel/ffmpeg/0016`), and with it the ffmpeg harness
  reaches its own software ceiling (141/147); without it such streams (HM-encoded
  material, not x265 output) decode against wrong references.
- **H.264** (JVT-AVC_V1): **129/135** on either decoder. The failures are flexible
  macroblock ordering and SP slices, which no decoder here does, and one
  left/top-crop-offset vector. FRExt (JVT-FR-EXT): **65/69** — 4:2:2 at 8- and 10-bit
  (`NV16`/`NV20`) decode correctly; the failures are 4:0:0 monochrome and MBAFF 4:2:2.
- **VP9** (VP9-TEST-VECTORS): **224/305** driver-attributable, the upstream series'
  exact number; the failure classes are sub-64x64 frames, mid-stream resolution
  changes, and chroma formats the hardware lacks.
- **VP8** (VP8-TEST-VECTORS): **59/61**, identical to the software score — zero
  hardware-attributable failures.
- **MPEG-2** (MPEG2_VIDEO-MAIN): progressive streams are bit-exact; interlaced streams
  diverge slightly (about 57–60 dB against the reference, accumulating over frames) on
  both the ffmpeg and GStreamer userspaces, so the divergence is the driver's.
  MPEG-1-syntax streams do not reach the stateless path at all.

The `*_rkmpp` decoders are compiled into `ffmpeg-rk` but never open — MPP finds no decode
client on a mainline kernel. Decode is `-hwaccel v4l2request`, for every codec above.

**AVS2 and AVS3 decode on the CPU, and only with the `avs-decode` feature.** The
silicon has no V4L2 route to either codec, so there is nothing to accelerate; what the
feature adds is `libdavs2` and `libuavs3d`, which Debian does not package and which
`ffmpeg-rk` is otherwise not configured against. Without it such a stream demuxes — the
`avs2` and `avs3` demuxers are always present — and then finds no decoder. The `avs` and
`cavs` entries a `-decoders` listing shows are a 1990s game FMV format and AVS1-P2
JiZhun; only AVS1 decodes without the feature.

Measured on this board, it is a 1080p capability. A demanding 1080p AVS2 stream decodes
at 49.2 fps and occupies 2.9 of the eight cores at realtime; 2160p AVS2 reaches 15.6 fps
with all eight busy, wanting 10.4 cores' worth, so it does not play at any thread count.
AVS3 is much cheaper — 113.3 fps at 1080p for 1.3 cores, about what software HEVC costs.
Both are bit-exact against their encoders' own reconstruction. Neither direction
encodes: FFmpeg has no AVS2 encoder wrapper at all, and its AVS3 one is for a library
this image does not carry.

```sh
boot2deb build turing-rk1/forky+media-accel-rockchip+avs-decode
```

### Encode

| Codec | Block | Depth | Reached as |
|---|---|---|---|
| H.264 | VEPU580 | 8-bit | `-c:v h264_rkmpp` |
| HEVC | VEPU580 | 8-bit | `-c:v hevc_rkmpp` |
| JPEG | VEPU121 | 8-bit | `-c:v mjpeg_v4l2m2m` |

H.264 and HEVC encode is the one part of the stack that does go through MPP, over the
vendor `/dev/mpp_service` the `060` patch adds.

The JPEG encoder is a separate mainline V4L2 M2M device (`/dev/video2`), and it takes a
different input set from the VEPU580: `yuv420p`, `nv12`, `yuyv422` or `uyvy422`, no
`drm_prime`. Pick the quality with `-quality` (5–100, larger is better) or with `-q:v`,
which takes the same percentage — note that is the **opposite** of the software `mjpeg`
encoder, where `-q:v` is a quantiser and lower means better, so `-q:v 5` here asks for
5% quality rather than near-lossless. Leaving both off keeps the driver's default of 50,
which lands a 1080p frame at 116 KB and 40.6 dB.

```sh
ffmpeg-rk -i input.mkv -frames:v 1 -c:v mjpeg_v4l2m2m -quality 80 -update 1 out.jpg
```

Two things to expect. **What it buys is CPU, not speed.** It spends 4.9 CPU-seconds per
300 1080p frames — a fixed-function cost, steady across content, most of what is left
being the copy of each frame into the V4L2 buffer — against 8.1 for a single-threaded
software encode of ordinary content and nearer 12 on high-detail material. But software
`mjpeg` threads across all eight cores while this path submits serially, so a 1080p
batch finishes about 2.4x *slower* in elapsed time for roughly half the CPU. That is
what you want for trickplay or snapshots running beside a transcode, and not what you
want if throughput is the goal. And at matched quality its files run 7–30% larger than
the software encoder's, which uses optimised Huffman tables where the hardware has fixed
ones.

It encodes up to 8176x8176, which is 511 macroblocks in each axis — the most the
hardware's nine-bit dimension registers hold, and the reason kernel patch `085` corrects
an advertised maximum of 8192 that produced a JPEG whose header said 0x0, silently, in
GStreamer as well as FFmpeg. Widths are rounded up to a multiple of 4 (854 becomes 856)
and the encoder says so.

The silicon has four of these cores; the driver registers the first and logs `missing
multi-core support, ignoring this instance` for the other three, so the limit is
throughput, not capability.

### Filtering

RGA does scale, colour conversion and alpha blending across three cores, as
`scale_rkrga`, `vpp_rkrga` and `overlay_rkrga`. Vulkan filters are available too when the
`vulkan` feature is selected, and `scale_vulkan` is the faster of the two for 8-bit work.

RGA is the only one of the two that handles 10-bit, because Vulkan cannot import `NV15`
at all. `overlay_rkrga` composites BGRA straight over a 10-bit decoded frame with no
scale stage in front — subtitle burn-in over 1080p HDR runs at 63.7 fps, and over 4K at
19.1 fps. The RGA filters need the RKMPP device named explicitly when the frames come
from `-hwaccel v4l2request`: pass `-init_hw_device rkmpp=rk -filter_hw_device rk`, or
they fail to configure with `No RKMPP hardware context provided`.

There is no hardware deinterlacer: RGA does not deinterlace, and `bwdif_vulkan`,
while it produces correct fields, costs the same CPU as the CPU filter and a third of
its throughput once the upload/download round-trips are paid (15 fps against 110 fps
at 1080i on this box). Deinterlace on the CPU with `bwdif`, and budget for it: a
hardware-decoded 1080i to 1080p60 transcode costs 2.8 of the 8 cores at realtime, where
the same transcode without deinterlacing costs 0.4. `bwdif=mode=send_frame` halves that
by emitting 30p instead of 60p. Standard-definition interlaced content is free — 576i to
576p50 runs at 12x realtime, about a fifth of a core.

### 10-bit, and why `NV15` is the whole story

Every 10-bit decode on this SoC produces **`NV15`** — packed 4:2:0, four samples in five
bytes, no padding. `rkvdec` declares no `P010` at all and cannot be asked for one, because
the hardware writes packed. So anything that wants a 10-bit frame has to consume `NV15`,
and exactly two things can:

- **RGA**, which converts it in hardware on the way out. This is the cheapest route and
  the only one that keeps the frame in hardware end to end.
- **`hwdownload`**, which brings it to system memory as a real `NV15` frame, where
  `format=yuv420p10le` unpacks it in swscale. This is what lets a GPU tone mapper see a
  hardware-decoded HDR frame:

  ```sh
  ffmpeg-rk -init_hw_device rkmpp=rk -filter_hw_device rk \
            -hwaccel v4l2request -hwaccel_output_format drm_prime -i hdr.mkv \
            -vf "scale_rkrga=w=1280:h=720,hwdownload,format=nv15,format=yuv420p10le,\
  libplacebo=tonemapping=bt.2390:format=nv12:w=1280:h=720" \
            -c:v hevc_rkmpp out.mp4
  ```

  Three things about that graph are load-bearing:

  **Scale on RGA first.** The unpack is a serial CPU stage, so its cost follows the
  frame size. Putting `scale_rkrga` in front hands swscale a 720p frame instead of a
  1080p one, which is most of the difference in the table below. `scale_rkrga` keeps
  `NV15` on the way out, so nothing is lost by doing it early.

  **`format=nv15` right after `hwdownload` is required.** The download only offers the
  frame's own format, so asking for `yuv420p10le` directly leaves the graph with no
  viable target; convert in a second step. `hwmap` does not substitute for the download
  here — deriving a Vulkan frames context from the mapped frame fails with `Unsupported
  pixel format: nv15`.

  **There is no `hwupload`, and that is deliberate.** A filtergraph carries one
  `-filter_hw_device`; `scale_rkrga` needs the RKMPP one, and an explicit `hwupload`
  into Vulkan would need the Vulkan one. They cannot both be satisfied —
  `hwupload=derive_device=vulkan` from an RKMPP device is `ENOSYS`, and naming Vulkan
  instead makes `scale_rkrga` fail with `No RKMPP hardware context provided`. The way
  out is to drop the filter: given no Vulkan device, `libplacebo` creates its own and
  does the upload and download itself, which leaves `-filter_hw_device` free for RGA.

  Measured on this board, 1080p HDR10 to 720p SDR over 900 frames, `user+sys`:

  | Route | CPU-s | fps |
  |---|---:|---:|
  | the chain above | **18.8** | **26.5** |
  | same, but unpacking at 1080p with an explicit `hwupload` | 32.8 | 21.9 |
  | software decode + `libplacebo` | 50.4 | 24.6 |
  | floor: hardware decode + RGA scale, no tone mapping | 1.8 | 335.2 |

  So the hardware path costs about a third of the CPU of software decode *and* runs
  faster. At 4K to 1080p the same chain is 12.5 CPU-s at 12.1 fps against 56.9 at 9.9 —
  under a quarter of the CPU — though 4K tone mapping is still short of realtime. The
  floor row is the point worth remembering: tone mapping, not decode, is what this
  chain spends its time on.

  Output correctness was checked, not assumed: this chain and the software-decode chain
  produce **bit-identical** frames, so the `NV15` unpack is exact.

**Vulkan cannot import `NV15`.** There is no VkFormat for a packed 10-bit 4:2:0 sample, so
the swscale unpack above is not avoidable, and the upload must be `yuv420p10le` — uploading
`p010le` produces a silently black frame rather than an error.

Both consumers take every 10-bit decoder's output at any width: the kernel pads the
decoded stride to 64 bytes on `rkvdec` (HEVC, H.264, VP9) and on the AV1 decoder
alike, which is what RGA's importer requires. The AV1 decoder's format list does
also offer `P010`, but nothing can reach it — the `v4l2request` hwaccel negotiates
`NV15`, and `hwdownload,format=p010le` on an AV1 frame is refused. That costs
nothing: P010 would spend 60% more write bandwidth per frame than NV15 for the same
samples.

### What the silicon has that this image does not drive

The tables above are what runs. The RK3588 carries more, and knowing which gaps are
whose saves chasing them:

- **AVS2 decode** is in the datasheet's decoder line, and **MPEG-4 ASP / H.263
  decode** is within the VDPU2-class block's reach. Neither has a V4L2 stateless
  uAPI at all, so no driver alone could add them; only the vendor stack decodes
  them in hardware. AVS2 and AVS3 are reachable in software with the `avs-decode`
  feature, at the cost measured above. MPEG-1 is nominally a subset of the MPEG-2 silicon, but MPEG-1-syntax
  streams never engage the stateless path in any userspace — software carries them,
  cheaply.
- **A dedicated JPEG decoder block** exists — the devicetree carries its clock and
  QoS plumbing — with a mainline driver in flight upstream. It is a stateful V4L2
  M2M device, so once it lands ffmpeg reaches it the same way the encoder above is
  reached; nothing new is needed beyond a decoder counterpart to that wrapper.
- **An IEP block** (the vendor kernels' hardware deinterlacer) has power-domain and
  QoS plumbing in the mainline devicetree and no mainline driver. Deinterlacing
  here is CPU `bwdif`, which the measurements above prefer over the Vulkan route
  anyway — at a real cost in cores for HD, and none for SD.
- **8K decode** is validated on this board for the `rkvdec` codecs: H.264, HEVC and
  VP9 decode 7680x4320 bit-exact at 8 *and* 10 bits, and `hevc_rkmpp` encodes 8K
  that decodes back cleanly. What made 10-bit look like a ceiling was the RCB SRAM
  mapping kernel patch `082` fixes — the decoder's line buffers were mapped into
  the DMA domain at an address the IOVA allocator did not know was taken, so a
  workload whose cumulative demand reached it collided and fell back to software.
  Three concurrent *8-bit* 8K streams hit the same address, which is what showed it
  was never about bit depth. Patch `084` raises the AV1 decoder's own limit from
  4096x2304 to 8192x4352 on the strength of that fix; AV1 above 4K has not yet been
  exercised on the board.
- **10-bit encode does not exist on this stack, or any other.** The VEPU580's own
  vendor userspace maps every 10-bit input format to "unsupported", so the 8-bit
  encode ceiling is structural — an HDR transcode tone-maps to SDR, which is what
  the chain above is for. VC-1 decode is similar: silicon the vendor stack itself
  declines to drive.
- **There is no VP8, VP9 or AV1 encoder** in the silicon; H.264, HEVC and JPEG are
  the complete hardware encode set.

For the codecs a media library actually holds — H.264, HEVC, VP9, AV1, VP8, MPEG-2,
at 8 and 10 bit — decode coverage on this image is complete, at conformance parity
with each driver's upstream results.
