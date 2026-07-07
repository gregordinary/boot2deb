# Turing RK1

The `turing-rk1-forky` recipe builds a bootable Debian **forky** image for the
[Turing RK1](https://turingpi.com/product/turing-rk1/) — an RK3588 compute module
that seats in a Turing Pi 2 cluster board. It pins kernel `v7.1.1` (linux-stable),
u-boot `v2026.04`, and the RGA / VEPU / VDPU hardware-transcode modules via the
`rk3588-accel` patch profile. It is a supported configuration in its own right and a
good starting point for any RK3588 board.

Build it exactly as in [Getting started](../getting-started.md):

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky
```

That produces `build/turing-rk1-forky/artifacts/turing-rk1.img.xz` — a whole-disk
image (GPT, u-boot in the reserved gap ahead of the first partition, then the ext4
rootfs), so a single write lays down everything, bootloader included.

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

- **grows the rootfs** to fill the whole medium (the 2 GB image expands to the disk's
  capacity), and
- **regenerates its SSH host keys.**

Log in as user **`debian`** with the password the build printed. It is expired, so you
are required to set a new one immediately. The `debian` account has passwordless
`sudo`, and the hostname is `turing-rk1`.

That is a booted Debian system. To confirm the hardware-transcode stack came up, check
for the RGA / VEPU / VDPU devices (`/dev/dri`, `/dev/rga`) and exercise ffmpeg's
`rkmpp` / `rkrga` paths.
