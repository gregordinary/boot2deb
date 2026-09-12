# Data volumes

A **data volume** is a second disk the image mounts for data and nothing else. No
part of the boot lives on it, so reflashing the OS costs nothing but the OS. The
new image finds the volume by label and adopts it.

## Why a board would want one

Most single-board machines have one medium their flashing route can write and one
medium with room on it, and they are not the same medium.

A Turing RK1 in a Turing Pi carrier is the clearest case. The BMC writes the
module's **eMMC**, and `tpi flash`, the web UI, and gadget mode all target it. The
loader the BMC streams into the module speaks eMMC and nothing else. The M.2
**NVMe** is invisible to that path.

Putting the root filesystem on the NVMe therefore means getting an image onto a disk
the management network cannot reach, which is a real errand:

1. Flash a whole OS to eMMC.
2. Boot it and copy the image across.
3. `dd` it to the NVMe.
4. Put a plain bootloader back on the eMMC.

The alternative is opening the case to get at the M.2 slot with an adapter.

Put the OS on the eMMC and the NVMe becomes data-only, and the errand disappears. So
does a second problem you might not have noticed you had. **Reimaging no longer
destroys the data.** With OS and data sharing one disk, every reinstall means
re-copying the library. With them split, `tpi flash` a new image and the library is
still there.

The same shape fits any board whose flashing route writes one medium. Examples are a
Chromebook's internal eMMC plus an SD card, or an SBC's SD card plus a USB disk.

If you *do* want root on the NVMe, that is the `split` layout plus a bootloader
that can write the disk — see [Turing RK1](boards/turing-rk1.md).

## Declaring one

**No shipped recipe declares a data volume**, and that is deliberate. Where the
data lives is a property of one installation, not of a board or of an application.
Two people running the same media server on one board can keep the library on an M.2
disk, an external USB drive, or network storage. Either can put root on either
medium. A recipe that guessed would send first boot looking for
hardware the
operator does not have.

So it is something you add to your own recipe — copy a shipped one and extend it,
as in [Adapting a shipped recipe](tutorials/adapting-a-recipe.md). There are two
halves, and a build fails if it has only one. The `data-volume` **feature** carries
the first-boot hook, and the recipe's `[[data_volumes]]` says what to mount where.

```toml
features = ["jellyfin", "media-accel-rockchip", "data-volume"]

[[data_volumes]]
match  = { kind = "nvme" }   # or { device = "/dev/nvme0n1" }
label  = "b2d-data"          # the identity that survives a reimage
mount  = "/srv"
fstype = "ext4"              # optional, the default
create = "if-blank"          # optional, the default; or "never"
```

| field | meaning |
| --- | --- |
| `match` | `{ kind = "nvme" \| "sata" \| "usb" \| "mmc" }` for the single disk of that transport, or `{ device = "/dev/..." }` for an exact one. A transport matching more than one disk is refused on the board rather than guessed. |
| `label` | Filesystem label, and the `LABEL=` the fstab entry mounts by. At most 16 bytes. **This is the volume's identity** — a later image with a different one sees a foreign disk and refuses it. |
| `mount` | Absolute path, never `/`. |
| `fstype` | `ext4`. |
| `create` | `if-blank` (default) to format a genuinely blank disk. `never` to only ever adopt a volume you prepared yourself. |

### How a disk is matched

`kind` names the **bus**, not the device-node spelling, because the spelling does
not separate the cases that matter. A SATA disk and a USB disk are both `/dev/sd*`.
A machine with an internal SSD and a drive somebody plugged in shows two devices no
name pattern can tell apart. Writing the wrong one is the accident worth designing
out.

So `sata` and `usb` are separate kinds, matched on the transport the kernel reports
(`lsblk -o TRAN`). A `/dev/sd*` whose transport cannot be read is skipped rather
than guessed at.

A disk must satisfy **both** the transport and the expected node name. That second
test is not redundant. On a board with eMMC, `lsblk` reports `mmcblk0boot0` and
`mmcblk0boot1` as whole disks (`TYPE=disk`), and those are the read-only eMMC boot
hardware partitions. The `mmcblk<n>` pattern excludes them, and read-only disks are
skipped besides. `mmc` is also the one kind that tolerates an unreported transport,
because the block driver commonly reports none and `mmcblk<n>` is unambiguous on its
own.

The disk holding root is never a candidate whatever `match` says. If a board's
transport is genuinely ambiguous, name the disk: `match = { device = "/dev/sda" }`.

`boot2deb resolve` prints what it resolved, so you can see what first boot will
look at before you flash:

```
data volume  : b2d-data on nvme -> /srv (ext4, create if blank)
```

## What first boot does, and does not do

Per volume, it stops at the first step that applies:

1. **Adopt** — a filesystem already carries the label. Mount it. **Never format.**
   This is the path every boot of a reflashed board takes.
2. **Create** — the matched disk is genuinely blank *and* `create = "if-blank"`.
   Write a GPT with one partition, `mkfs` with the label, mount.
3. **Refuse** — anything else. Log what was found and leave the disk alone.

"Genuinely blank" is three independent checks: no partition table, no filesystem
signature, and no partitions the kernel already sees. The disk holding root is
never a candidate whatever `match` says.

Step 3 is the point of the feature, not a safety net bolted onto it. A volume this
image did not create is evidence of data someone wants, so it is left alone and the
reason is logged. If that is still more latitude than you want, `create = "never"`
removes the blank-disk case too.

The `/etc/fstab` entry is written at **build** time, so every boot after the first
mounts the volume with no hook involved. It carries `nofail` and a short device
timeout. A data disk that is absent, dead, or slow to enumerate must never be the
reason a system will not boot. A node with no second disk boots normally and simply
has an empty mount point.

## Renaming a volume

The label is the identity, so changing it in a recipe without changing it on the
disk means the next image refuses the volume. That is safe, but not what you meant.
Relabel the disk on the board in the same change:

```sh
sudo umount /srv
sudo e2label /dev/nvme0n1p1 new-label
```
