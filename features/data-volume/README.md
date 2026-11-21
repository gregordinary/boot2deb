# `data-volume` — a second disk for data, kept across reimaging

Mount a disk that holds no part of the boot, so that reflashing the OS costs
nothing but the OS.

## The layout it exists for

Put the whole bootable system — bootloader, kernel, rootfs — on the medium the
board's flashing route can actually write, and keep the large disk for data only.

On a Turing RK1 in a Turing Pi carrier that is eMMC + M.2 NVMe, and it resolves
the awkwardness of the alternative: the BMC writes the module's eMMC and nothing
else, so putting root on the NVMe means getting an image onto a disk the
management path cannot reach. With the OS on eMMC that problem disappears — and
the more valuable property arrives with it. `tpi flash` a new image and the data
is still there, because the new image finds the volume by label and adopts it.

The same shape fits any board whose flashing route writes one medium: a
Chromebook's internal eMMC plus an SD card, an SBC's SD card plus a USB disk.

## How to use it

Two halves, and resolution rejects either without the other: this feature includes
the first-boot hook, and the recipe says what to mount where.

```toml
features = ["data-volume"]

[[data_volumes]]
match  = { kind = "nvme" }   # or { device = "/dev/nvme0n1" }
label  = "b2d-data"          # the identity that survives a reimage
mount  = "/srv"
fstype = "ext4"              # optional, the default
create = "if-blank"          # optional, the default; or "never"
```

`match` is either a transport (`nvme`, `sata`, `usb`, `mmc`) or an exact device
node. A transport matching more than one disk is refused on the board rather than
guessed — name the disk explicitly when a board has two.

`sata` and `usb` are separate kinds because they share the `/dev/sd*` name: only
the transport the kernel reports separates an internal disk from a drive somebody
plugged in, so a `/dev/sd*` whose transport cannot be read is skipped rather than
guessed at. A disk must match both the transport and the expected node name, which
is what keeps `mmcblk<n>boot<n>` — the read-only eMMC boot hardware partitions,
which `lsblk` also reports as whole disks — out of the `mmc` kind.

The build writes the `/etc/fstab` entry, so every boot after the first mounts the
volume with no hook involved. The entry contains `nofail` and a short device
timeout: a data disk that is absent, dead, or slow to enumerate must never be the
reason a system will not boot.

## What the hook will and will not do

On first boot, per volume, it stops at the first step that applies:

1. **Adopt** — a filesystem already has the label. Mount it. **Never format.**
   This is the path every boot of a reflashed board takes.
2. **Create** — the matched disk is proven blank *and* `create = "if-blank"`.
   Write a GPT with one partition, `mkfs` with the label, mount.
3. **Refuse** — anything else. Log what was found and leave the disk alone.

"Proven blank" is a set of facts a tool reported before the first write, because
the cost of a false positive is somebody's data. A query that fails, prints nothing,
or needs a tool the image lacks is a refusal, never a blank:

- `match` names exactly one writable whole disk: `lsblk` reports `TYPE=disk` and
  `RO=0`, and sysfs has no `partition` attribute for its device number.
- The disk is not under root. The hook walks root's chain of devices through every
  device-mapper, md or LUKS layer down to the disks, and compares device numbers,
  not names. When that chain cannot be walked, no volume is created on that boot.
- Nothing is on or above it: `lsblk` lists the disk alone and unmounted, and nothing
  holds it.
- Its first and last MiB read back.
- `wipefs` lists no signature: no partition table, no filesystem, no RAID or LUKS
  header. `wipefs` reports every signature it finds, colliding ones included, and
  fails outright on a device it cannot open.

Step 3 is the point of the whole feature. A volume this image did not create is
evidence of data someone wants. Set `create = "never"` to remove even the
blank-disk case and prepare the volume by hand.

## Re-verifying the refusal

The claim worth testing is the negative one, so the test asserts on whether
`sfdisk`/`mkfs.ext4` were reached at all rather than on log text. Every refusing
case changes one fact about a world in which the hook does create the volume, and
covers a failed or empty probe, an absent tool, a partition or device-mapper node
named as the disk, and a root disk under LUKS. It stubs the block tools and touches
no real device, and `cargo test` runs it:

```sh
sh features/data-volume/test-ladder.sh    # from the boot2deb root
```

## Changing the label

The label *is* the volume's identity. An image built with a different one sees an
unlabelled foreign disk and refuses it — safe, but not what you meant. To rename,
relabel the volume on the board (`e2label`) in the same change as the recipe.
