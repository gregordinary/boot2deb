# Producing images

`boot2deb press` turns a build's artifacts into the file you hand a flasher:
one master, many cards, each optionally personalized and given identifiers of
its own.

```sh
boot2deb press turing-rk1/forky card.img
```

The command resolves the recipe exactly as `build` does and derives the rest
from it. A `combined` build presses one whole-disk image, and a u-boot
deliverable presses its standalone boot image. A `--layout split` build is two
files for two media: `--boot-out` for the eMMC/SPI half, `--rootfs-out` for the
OS disk. A split build pressed with one positional output is an error naming
both flags, not a wrong file.

A press reads the build's artifacts through the point's artifact inventory,
`<stem>.artifacts.toml`, which the build writes as each stage finishes. It takes
a file only from the entry of the stage that published it, and only under three
conditions:

- The entry was built under the lock the recipe has now.
- For an image, the entry's layout is the one the press resolves.
- The file still holds the bytes the entry records.

A file in the artifact directory that the inventory does not list is never
pressed, whatever its name. A container an earlier build left behind, or a
rootfs tar from before a re-pin, therefore cannot stand in for the current one.
A press over a stale or partial build fails naming the stage to run again.

A plain press streams the build's compressed artifact into the output, so
pressing a card never costs a rebuild. On the way through, it holds the artifact
to its recorded digest, and the disk it decompresses to to the build's recorded
disk digest.

It then verifies the file it wrote. The bytes are re-read against that digest.
The partition table is read back and compared entry-for-entry against the one
the artifact contains. An artifact that contains a rootfs and has no readable
table fails, since only a boot image has none to compare. What you hand the
flasher is what the build made. `--dry-run` prints what would be
pressed, including the medium size the image needs, without writing anything.

Beside each output that contains a rootfs, `press` writes its
[file manifest](reference/reproducibility.md#the-rootfs-file-manifest), named
for the output: `card.img` gets `card.rootfs.uapi16`. A plain press copies the
build's, since the rootfs is the build's to the byte. A press with additions
formats a new rootfs, so it writes a manifest of that one. Seed keys are written to
the seed partition, outside the rootfs, and leave the manifest true.

Every file a press writes is checked as one set before any of it is written.
That is each image, its file manifest, and for a re-assembled image its
credentials file (below):

- A destination must be a regular file or not exist yet. A symlink is not
  written through, and a block device, directory or FIFO is refused by kind:
  boot2deb does not write devices.
- A destination that exists is replaced only with `--overwrite`.
- Two destinations never name the same file, by path or through a link. So
  `--boot-out` and `--rootfs-out` cannot both name one file.
- No destination is, or links to, anything the press reads, or one of the build's
  own names in its artifact directory. The press reads the build's artifacts, the
  `--copy` and `--deb` sources, and the files under a `--copy-tree` directory.

Each output is then produced under a temporary name in its own directory. Only
once every output is written, verified and personalized do they take their
names. A press that fails leaves no partial output, and an existing file is
replaced whole rather than written into.

A re-assembled image gets a first-boot password of its own. The press writes it
to `<output stem>.credentials.toml` beside the image (`card.img` gets
`card.credentials.toml`), readable by you only, and prints the file's path, not
the password. A streamed image is the build's, password included, so its
password is in the build's own credentials file. See
[The password](access.md#the-password).

## Flashing the pressed file

boot2deb does not write devices. The pressed image is an ordinary raw disk
image, so write it with whatever flasher you trust:

- **Pyrographer** — a flasher with a plan→confirm→write gate, device safety
  checks, and native rockusb.
- **Plain `dd`**, the universal fallback:

  ```sh
  lsblk    # confirm the device first; dd overwrites it whole
  sudo dd if=card.img of=/dev/sdX bs=4M status=progress conv=fsync
  ```

- A board's own route — the Turing Pi BMC (`tpi flash -n 2 -l -i card.img` or
  the web UI), a vendor tool — anything that writes a raw image.

Boards' pages say which media each board boots from. `build` also prints the
matching `dd` line for the artifact it just made.

## Per-unit personalization

One image, six boards, no collisions:

```sh
boot2deb press turing-rk1/forky rk1-03.img \
    --hostname rk1-03 --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
```

Every image contains a 1 MiB FAT partition labeled `b2d-seed` holding a
`seed.txt` of `key=value` lines. `press` regenerates the whole partition with
what you name — never edits it in place — and the device applies it once at
first boot:

| key | flag | what the device does |
| --- | --- | --- |
| `hostname=` | `--hostname` | `hostnamectl` plus `/etc/hosts` |
| `authorized_key=` | `--ssh-key` (repeatable) | appends to the default account's `authorized_keys` |
| `wifi_ssid=` | `--wifi-ssid` | writes a NetworkManager connection profile and joins the network |
| `wifi_psk=` | `--wifi-psk` | the WPA passphrase for `wifi_ssid`, omitted for an open network |
| `static_ip=` | `--static-ip` | pins a static IPv4 (`address/prefix[,gateway[,dns...]]`) on the connection the seed sets up |

The Wi-Fi keys are the canonical per-site values that must never sit in a
committed recipe. They apply on images that contain NetworkManager, which every
Wi-Fi-capable board's image does, and degrade to a logged skip elsewhere. Like
every seed key, they are plain text in the seed partition, which personalizes
a unit rather than keeping secrets. An image pressed with no keys contains the
empty template and behaves exactly as an unpersonalized image always has.

`static_ip=` follows the connection the other keys define. Seeded together
with `wifi_ssid` it makes that Wi-Fi profile static, and seeded alone it pins
the wired interface. Where the image contains NetworkManager it is written as a
connection profile, and on an Ubuntu image as a netplan file. Where the image runs
dhcpcd it goes into `dhcpcd.conf`, and anywhere else it is a logged skip.

A netplan file is first generated together with the image's own definitions, and
only a file that generates is installed. A configuration that netplan or dhcpcd
then fails to apply is taken out again. The wired interfaces stay on DHCP, and the
log says the address was not applied.

Fields beyond the address are optional. No gateway field means no default route
is written, and no DNS fields mean no resolvers are. The key personalizes
exactly what it names. Only the syntax is validated (IPv4 dotted quads, a
`/1`–`/32` prefix), at press time and again on the device, and the address plan
is yours.

Because the seed is FAT, an operator can also edit it with no tooling at all.
Plug the card into any laptop, open `seed.txt` on the `B2D-SEED` volume,
change the hostname, and eject. The file documents its own keys. The device
holds an edited file to the rules `press` checks. A value that `press` refuses
is skipped whole and logged, and the other keys still apply.

To re-personalize a pressed **file** without re-pressing it:

```sh
boot2deb seed rk1-03.img --hostname rk1-04
```

`seed` takes no recipe — the seed partition is found by its GPT label, so the
file is the whole input. With no keys it resets the seed to the empty
template. It refuses block devices (boot2deb does not write them). A card that
is already written is re-personalized by editing `seed.txt` directly.

The first-boot password stays per *image*, not per unit. Boards pressed from
one streamed artifact share that build's expired password, and `--ssh-key` is
the answer to a fleet. (A press that re-assembles draws a fresh password of its
own, printed when it happens.)

## Giving a medium its own identifiers

Every build of a recipe stamps the same identifiers into its image:

- The GPT disk GUID
- The root partition's PARTUUID, which the kernel command line roots on
- The ext4 UUID
- The seed volume serial

Every medium written from that image therefore answers to the same root
PARTUUID. That is harmless until two of them sit in one machine, such as a USB
stick and the eMMC install written from the same recipe. The initramfs roots on
the first partition it finds with that PARTUUID, which can be the other disk's.
The stick's kernel then runs the eMMC's system.

`--fresh-identity` gives the pressed image identifiers no other image has:

```sh
boot2deb press asus-c201/mainline-forky stick.img --fresh-identity
```

The press draws them at random and re-assembles the image around them. The build
wrote the root PARTUUID into the rootfs, and the press rewrites each place:

| File | Boards | What the press changes |
| --- | --- | --- |
| `/etc/fstab` | every board | the root entry, and the EFI system partition's under the `efi` loader |
| `/boot/extlinux/extlinux.conf` | the `extlinux` loader | the `root=` of every boot entry |
| `/boot/grub/grub.cfg` | the `efi` loader | the `root=` of every boot entry, and the filesystem UUID GRUB searches for |
| `/boot/depthcharge/*.img` | `depthcharge` | the signed kernel, re-signed for the new root and placed in the kernel slot too |

Under the `efi` loader the EFI system partition gets a fresh GUID and volume serial as
well. Its files name no identifier: GRUB's core image finds the rootfs as a partition of
the disk it was loaded from. The press therefore formats it again unchanged.

A depthcharge kernel is re-signed with the key the image contains for its own
kernel upgrades, the vboot developer key. The press refuses a kernel that any
other key signed. It then reads every file under `/etc` and `/boot` for the
build's identifiers. When one still names them, the press refuses the image.

`--embed-image` implies `--fresh-identity`. Its card stays in the machine beside
the disk `boot2deb-install-to` writes, and the two must not share a root
PARTUUID.

The press draws the identifiers once, and the medium keeps them for life. The
image records the draw in its `[pressed]` table (see
[Image identity](reference/image-identity.md#pressed-marks-a-derived-image)). A
press with a fresh identity re-assembles, so it needs the build's kept
artifacts, as a press with additions does.

## Tree additions

What belongs to a unit or a site, rather than to the recipe? A recipe
describes every board of a kind, and `press` stamps one card. Additions put
arbitrary files into the pressed image's filesystem:

```sh
boot2deb press turing-rk1/forky site.img \
    --copy site.conf:/etc/myapp/site.conf \
    --deb ~/build/myapp_1.2_arm64.deb
```

- **`--copy SRC:DEST`** (repeatable) — a host file placed at an absolute path
  in the tree: a site config, a one-off script, a data file. Mode `0644`
  (`0755` when the source is executable), owner root. Missing parent
  directories are created. Copying over a shipped file replaces it.
- **`--copy-tree DIR`** (repeatable) — a whole directory that **mirrors the
  target rootfs**: `DIR/etc/myapp/site.conf` is placed at `/etc/myapp/site.conf`.
  See [A directory that mirrors the rootfs](#a-directory-that-mirrors-the-rootfs).
- **`--deb PATH`** (repeatable) — a local `.deb` staged into
  `/var/lib/boot2deb/firstboot-debs/`, installed at first boot with `dpkg -i`
  (alphabetical). The honest caveat is that `dpkg -i` resolves nothing. A
  dependency not already in the image leaves the package unconfigured until
  `apt-get -f install` can run. The hook attempts that only if the board has
  network by then. The use case is the locally-built, self-contained deb you
  are iterating on.
- **`--embed-image`** — see [Installing to internal storage](#installing-to-internal-storage).

### A directory that mirrors the rootfs

Per-site customization of any size is a directory, not a stack of flags:

```sh
boot2deb press turing-rk1/forky rk1-03.img --copy-tree ./site
```

```
site/
  etc/
    myapp/
      site.conf              ->  /etc/myapp/site.conf
      node.conf.tmpl         ->  /etc/myapp/node.conf   (expanded, see below)
    current.conf -> /etc/myapp/site.conf   (stays a symlink)
  usr/local/bin/
    site-hello               ->  /usr/local/bin/site-hello  (0755, it is executable)
```

Every **regular file and symlink** under `DIR` is placed at its corresponding
absolute path. Directories are not placed as entries of their own. The parents
each file needs are created root-owned `0755`, so your site tree's umask never
reaches the image. Files are created `0644`, or `0755` when they are executable on the
host, exactly as `--copy` does. A symlink is recorded as a symlink and never
followed, so a link pointing outside the tree is placed as the link it is.

The refusals are per file and name what they found:

- A destination in the reserved set (`/etc/shadow`, `/etc/boot2deb/image.toml`)
- A path under `/dev`
- An entry that is neither a regular file, a symlink, nor a directory

Anything else — a device node, a FIFO, a socket, a hard link — is refused rather
than silently dropped. A directory that names no files at all is an error too. A
`--copy-tree` that quietly added nothing would leave you with a plain streamed
image.

There is no `patch.sh`, and that is deliberate. `--copy-tree` places files, and
anything needing logic rather than placement belongs in a feature, where it runs
with package resolution and maintainer scripts behind it.

### Files that depend on the image: `*.tmpl`

A copy is byte-for-byte, which cannot express a file whose content depends on
the image it is written to. A source named `*.tmpl` is a **template**: its
`{{image.<name>}}` references are expanded at press time and it is placed without
the suffix.

```
# site/etc/myapp/node.conf.tmpl        # /etc/myapp/node.conf, in the image
node_id  = {{image.hostname}}          node_id  = rk1-03
root_dev = PARTUUID={{image.rootfs_partuuid}}
                                       root_dev = PARTUUID=16e51e55-5916-...
built    = {{image.recipe}}            built    = turing-rk1/forky
```

The point of it is the identifiers. `rootfs_partuuid`, `rootfs_uuid` and the
rest are *derived by boot2deb* from the recipe. Without a template, naming one
in a config file would mean pressing the image, reading its GPT back, editing,
and pressing again. Everything else in the set is a convenience.

The vocabulary is **the image's identity**, and it is closed. Every name is a
field of the `/etc/boot2deb/image.toml` the image contains, or one of the
identifiers stamped into its GPT and superblock:

| reference | what it expands to |
| --- | --- |
| `{{image.hostname}}` | the name this unit will answer to: the `--hostname` seed key when the press names one, else the recipe's |
| `{{image.device}}` | board slug (`turing-rk1`) |
| `{{image.description}}` | the board's human-readable description |
| `{{image.arch}}` | Debian architecture (`arm64`) |
| `{{image.soc}}` | SoC slug (`rk3588`) |
| `{{image.boot_method}}` | boot method (`rockchip-rkbin`) |
| `{{image.suite}}` | Debian suite (`forky`) |
| `{{image.layout}}` | `combined` or `split` |
| `{{image.kernel}}` | kernel definition id (`rk3588-mainline-7.3`) |
| `{{image.recipe}}` | the build point pressed (`turing-rk1/forky`) |
| `{{image.rootfs_partuuid}}` | the rootfs partition's PARTUUID, hyphenated — the form `root=PARTUUID=` and `/etc/fstab` take |
| `{{image.rootfs_uuid}}` | the rootfs ext4 superblock UUID, hyphenated — the form `UUID=` takes |
| `{{image.seed_partuuid}}` | the seed partition's PARTUUID, hyphenated |
| `{{image.disk_guid}}` | the GPT header's disk GUID, hyphenated |

A name outside the set is an **error at press time**, listing the whole
vocabulary. It is never an empty string in a shipped config that only fails on
the board. Names are checked when the flags are parsed, so a typo fails before
any artifact is read.

`{{` is claimed **only** when `image.` follows it. A file that contains braces
of its own, such as a Go, Helm, or Jinja template shipped as data, passes
through untouched. The flip side is that a mistyped namespace
(`{{iamge.suite}}`) is literal text rather than an error. That is why `press`
reports the reference count per template: expecting four expansions and being
told three is how you find it.

```
template -> /etc/myapp/node.conf (3 reference(s))
```

`--copy` honors the same suffix, so `--copy node.conf.tmpl:/etc/myapp/node.conf`
expands. The destination there is exactly what you named. The suffix decides a
destination only in a tree, where the destination is derived. To ship a file
that really is called `*.tmpl`, name it `foo.tmpl.tmpl`: it expands and is placed as
`foo.tmpl`. A template must be UTF-8 and is read whole to be parsed, so it is
capped at 1 MiB. Drop the suffix to copy a large file verbatim.

A press with additions cannot stream, and neither can one with
`--fresh-identity`. It **re-assembles** the image from the build's kept
artifacts (the rootfs tar, the boot payloads), merging the additions into the
filesystem before it is formatted. The build must have run
on this machine, and the recipe's artifacts are read, never modified. Under a
`fit`-sized recipe the filesystem grows to hold whatever was added. Under a
fixed `image_size` a press that does not fit fails in the format. The
re-assembled rootfs passes the same verification a build's does (the
in-process scan, plus `e2fsck -fn` where the host has it).

Additions do not re-run package resolution or maintainer scripts. Anything
that needs those is a build — the recipe and feature path exists for it.

### What a pressed image says about itself

A re-assembled image is **derived, not canonical**. The recipe's artifacts
and their provenance stay untouched. The pressed file records its own ancestry
in `/etc/boot2deb/image.toml` as a `[pressed]` table. That table holds the
source artifact stem and what was added, by kind and destination, never by
content. A press that drew the image's identifiers adds
`fresh_identity = true`.

A tree's entries are recorded there one destination at a time, exactly as a
`--copy` is. A template is recorded by the name it was placed under, rather than
the one it was authored as.
`reproduce` reproduces builds, not pressings. The seed partition is not
summarized there: it is self-describing, and `boot2deb seed` can rewrite it
later without touching the filesystem.

## Installing to internal storage

Some boards boot from removable media but live on internal storage — boot a
Chromebook from SD, install to its eMMC. `--embed-image` places the recipe's
own compressed artifact inside the pressed image, at
`/var/lib/boot2deb/install/`:

```sh
boot2deb press asus-c201/forky card.img --embed-image
```

Beside the artifact, the press places `<artifact>.info`: its sha256 and the
length and sha256 of the disk it decompresses to, as the build recorded them.

On the booted board, `boot2deb-install-to` (in every image) writes the
embedded artifact to the internal disk. It wraps the documented `dd` procedure
with the checks that matter, and refuses whenever it cannot establish an answer:

- The payload must hash to its record before the target is touched, so a card
  that was damaged after pressing never half-writes a disk.
- The target must be a whole, writable disk, at least as large as the image.
- It must not be or hold anything the running system's root sits on, through any
  LVM or LUKS layer.
- Nothing on it can be mounted, and no other device can hold it.
- The confirmation requires typing the device's name.

After the write, a decompressor or `dd` failure is reported as a failed install.
The disk is then read back and held to the recorded image digest, and only then
is the install reported done.

```sh
sudo boot2deb-install-to /dev/mmcblk0
```

The pressed card is a derived copy that *contains* the artifact. The embedded
image is the artifact itself, byte for byte. The card gets a
[fresh identity](#giving-a-medium-its-own-identifiers), so the installed disk
and the card that installed it never share a root PARTUUID. `--embed-image`
needs a combined-layout recipe built with compression on (the default).

## A split build

```sh
boot2deb press turing-rk1/forky --layout split \
    --boot-out emmc-boot.img --rootfs-out nvme.img
```

The boot image goes to the medium the board boots from (eMMC or SPI), and the
rootfs image to whatever disk the OS lives on. The seed, and any addition,
goes into the rootfs image, so personalization is written to the disk the OS reads. The
boot image is streamed unchanged.
