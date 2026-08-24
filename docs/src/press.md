# Producing images

`boot2deb press` turns a build's artifacts into the file you hand a flasher:
one master, many cards, each optionally stamped with its own identity.

```sh
boot2deb press turing-rk1/forky card.img
```

The command resolves the recipe exactly as `build` does and derives the rest
from it: a `combined` build presses one whole-disk image, a u-boot deliverable
presses its standalone boot image, and a `--layout split` build is two files
for two media — `--boot-out` for the eMMC/SPI half, `--rootfs-out` for the OS
disk. A split build pressed with one positional output is an error naming both
flags, not a wrong file.

A plain press streams the build's compressed artifact into the output —
pressing a card never costs a rebuild — and then verifies the file it wrote:
the bytes are re-read against the digest computed while streaming, and the
partition table is read back and compared entry-for-entry against the one the
artifact carries. What you hand the flasher is what the build made.
`--dry-run` prints what would be pressed, including the medium size the image
needs, without writing anything.

## Flashing the pressed file

boot2deb does not write devices. The pressed image is an ordinary raw disk
image; write it with whatever flasher you trust:

- **pyrographer** — a flasher with a plan→confirm→write gate, device safety
  checks, and native rockusb.
- **Plain `dd`**, the universal fallback:

  ```sh
  lsblk    # confirm the device first; dd overwrites it whole
  sudo dd if=card.img of=/dev/sdX bs=4M status=progress conv=fsync
  ```

- A board's own route — the Turing Pi BMC (`tpi flash -n 2 -l -i card.img` or
  the web UI), a vendor tool — anything that writes a raw image.

Boards' pages say which media each board boots from; `build` also prints the
matching `dd` line for the artifact it just made.

## Per-unit personalization

One image, six boards, no collisions:

```sh
boot2deb press turing-rk1/forky rk1-03.img \
    --hostname rk1-03 --ssh-key "$(cat ~/.ssh/id_ed25519.pub)"
```

Every image carries a 1 MiB FAT partition labeled `b2d-seed` holding a
`seed.txt` of `key=value` lines. `press` regenerates the whole partition with
what you name — never edits it in place — and the device applies it once at
first boot:

| key | flag | what the device does |
| --- | --- | --- |
| `hostname=` | `--hostname` | `hostnamectl` plus `/etc/hosts` |
| `authorized_key=` | `--ssh-key` (repeatable) | appends to the default account's `authorized_keys` |
| `wifi_ssid=` | `--wifi-ssid` | writes a NetworkManager connection profile and joins the network |
| `wifi_psk=` | `--wifi-psk` | the WPA passphrase for `wifi_ssid`; omit it for an open network |
| `static_ip=` | `--static-ip` | pins a static IPv4 (`address/prefix[,gateway[,dns...]]`) on the connection the seed sets up |

The Wi-Fi keys are the canonical per-site values that must never sit in a
committed recipe. They apply on images that carry NetworkManager — every
Wi-Fi-capable board's does — and degrade to a logged skip elsewhere; like
every seed key, they are plain text in the seed partition, which personalizes
a unit rather than keeping secrets. An image pressed with no keys carries the
empty template and behaves exactly as an unpersonalized image always has.

`static_ip=` follows the connection the other keys define: seeded together
with `wifi_ssid` it makes that Wi-Fi profile static, and seeded alone it pins
the first wired interface — through NetworkManager where the image carries it,
through `dhcpcd.conf` where it carries dhcpcd, with a logged skip where it
carries neither. Fields beyond the address are optional: no gateway field
means no default route is written, no DNS fields mean no resolvers are — the
key personalizes exactly what it names. Only the syntax is validated at press
time (IPv4 dotted quads, a `/1`–`/32` prefix); the address plan is yours.

Because the seed is FAT, an operator can also edit it with no tooling at all:
plug the card into any laptop, open `seed.txt` on the `B2D-SEED` volume,
change the hostname, eject. The file documents its own keys.

To re-personalize a pressed **file** without re-pressing it:

```sh
boot2deb seed rk1-03.img --hostname rk1-04
```

`seed` takes no recipe — the seed partition is found by its GPT label, so the
file is the whole input. With no keys it resets the seed to the empty
template. It refuses block devices (boot2deb does not write them); a card that
is already written is re-personalized by editing `seed.txt` directly.

The first-boot password stays per *image*, not per unit: boards pressed from
one streamed artifact share that build's expired password, and `--ssh-key` is
the answer to a fleet. (A press with additions re-assembles, and so draws a
fresh password of its own — printed when it happens.)

## Tree additions

What belongs to a unit or a site, rather than to the recipe? A recipe
describes every board of a kind; `press` stamps one card. Additions put
arbitrary files into the pressed image's filesystem:

```sh
boot2deb press turing-rk1/forky site.img \
    --copy site.conf:/etc/myapp/site.conf \
    --deb ~/build/myapp_1.2_arm64.deb
```

- **`--copy SRC:DEST`** (repeatable) — a host file placed at an absolute path
  in the tree: a site config, a one-off script, a data file. Mode `0644`
  (`0755` when the source is executable), owner root; missing parent
  directories are created. Copying over a shipped file replaces it.
- **`--copy-tree DIR`** (repeatable) — a whole directory that **mirrors the
  target rootfs**: `DIR/etc/myapp/site.conf` lands at `/etc/myapp/site.conf`.
  See [A directory that mirrors the rootfs](#a-directory-that-mirrors-the-rootfs).
- **`--deb PATH`** (repeatable) — a local `.deb` staged into
  `/var/lib/boot2deb/firstboot-debs/`, installed at first boot with `dpkg -i`
  (alphabetical). The honest caveat: `dpkg -i` resolves nothing, so a
  dependency not already in the image leaves the package unconfigured until
  `apt-get -f install` can run — which the hook attempts only if the board has
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
absolute path. Directories are not placed as entries of their own — the parents
each file needs are created root-owned `0755`, so your site tree's umask never
reaches the image. Files land `0644`, or `0755` when they are executable on the
host, exactly as `--copy` does; a symlink is recorded as a symlink and never
followed, so a link pointing outside the tree lands as the link it is.

The refusals are per file and name what they found: a destination the reserved
set owns (`/etc/shadow`, `/etc/boot2deb/image.toml`), a path under `/dev`, or an
entry that is neither a regular file, a symlink, nor a directory — a device
node, FIFO, socket, or hard link is refused rather than silently dropped. A
directory that names no files at all is an error too, since a `--copy-tree` that
quietly added nothing would leave you with a plain streamed image.

There is no `patch.sh`, and that is deliberate: `--copy-tree` places files, and
anything needing logic rather than placement belongs in a feature, where it runs
with package resolution and maintainer scripts behind it.

### Files that depend on the image: `*.tmpl`

A copy is byte-for-byte, which cannot express a file whose content depends on
the image it lands in. A source named `*.tmpl` is a **template**: its
`{{image.<name>}}` references are expanded at press time and it lands without
the suffix.

```
# site/etc/myapp/node.conf.tmpl        # /etc/myapp/node.conf, in the image
node_id  = {{image.hostname}}          node_id  = rk1-03
root_dev = PARTUUID={{image.rootfs_partuuid}}
                                       root_dev = PARTUUID=16e51e55-5916-...
built    = {{image.recipe}}            built    = turing-rk1/forky
```

The point of it is the identifiers. `rootfs_partuuid`, `rootfs_uuid` and the
rest are *derived by boot2deb* from the recipe, so without a template naming one
in a config file would mean pressing the image, reading its GPT back, editing,
and pressing again. Everything else in the set is a convenience.

The vocabulary is **the image's identity** — every name is a field of the
`/etc/boot2deb/image.toml` the image carries, or one of the identifiers stamped
into its GPT and superblock — and it is closed:

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
| `{{image.kernel}}` | kernel definition id (`rk3588-mainline-7.2`) |
| `{{image.recipe}}` | the build point pressed (`turing-rk1/forky`) |
| `{{image.rootfs_partuuid}}` | the rootfs partition's PARTUUID, hyphenated — the form `root=PARTUUID=` and `/etc/fstab` take |
| `{{image.rootfs_uuid}}` | the rootfs ext4 superblock UUID, hyphenated — the form `UUID=` takes |
| `{{image.seed_partuuid}}` | the seed partition's PARTUUID, hyphenated |
| `{{image.disk_guid}}` | the GPT header's disk GUID, hyphenated |

A name outside the set is an **error at press time**, listing the whole
vocabulary — never an empty string in a shipped config that only fails on the
board. Names are checked when the flags are parsed, so a typo fails before any
artifact is read.

`{{` is claimed **only** when `image.` follows it, so a file that carries braces
of its own — a Go, Helm, or Jinja template shipped as data — passes through
untouched. The flip side is that a mistyped namespace (`{{iamge.suite}}`) is
literal text rather than an error, which is why `press` reports the reference
count per template: expecting four expansions and being told three is how you
find it.

```
template -> /etc/myapp/node.conf (3 reference(s))
```

`--copy` honours the same suffix — `--copy node.conf.tmpl:/etc/myapp/node.conf`
expands — but the destination there is exactly what you named; the suffix
decides a destination only in a tree, where the destination is derived. To ship
a file that really is called `*.tmpl`, name it `foo.tmpl.tmpl`: it expands and
lands as `foo.tmpl`. A template must be UTF-8 and is read whole to be parsed, so
it is capped at 1 MiB — drop the suffix to copy a large file verbatim.

A press with additions cannot stream: it **re-assembles** the image from the
build's kept artifacts (the rootfs tar, the boot payloads), merging the
additions into the filesystem before it is formatted. The build must have run
on this machine; the recipe's artifacts are read, never modified. Under a
`fit`-sized recipe the filesystem grows to hold whatever was added; under a
fixed `image_size` a press that does not fit fails in the format. The
re-assembled rootfs passes the same verification a build's does (the
in-process scan, plus `e2fsck -fn` where the host has it).

Additions do not re-run package resolution or maintainer scripts. Anything
that needs those is a build — the recipe and feature path exists for it.

### What a pressed image says about itself

A pressed image with additions is **derived, not canonical**. The recipe's
artifacts and their provenance stay untouched; the pressed file records its
own ancestry in `/etc/boot2deb/image.toml` as a `[pressed]` table — the source
artifact stem and what was added, by kind and destination, never by content. A
tree's entries are recorded there one destination at a time, exactly as a
`--copy` is, and a template by the name it landed under rather than the one it
was authored as.
`reproduce` reproduces builds, not pressings. The seed partition is not
summarized there: it is self-describing, and `boot2deb seed` can rewrite it
later without touching the filesystem.

## Installing to internal storage

Some boards boot from removable media but live on internal storage — boot a
Chromebook from SD, install to its eMMC. `--embed-image` carries the recipe's
own compressed artifact inside the pressed image, at
`/var/lib/boot2deb/install/`:

```sh
boot2deb press asus-c201/forky card.img --embed-image
```

On the booted board, `boot2deb-install-to` (in every image) writes the
embedded artifact to the internal disk, wrapping the documented `dd` procedure
with the checks that matter: the target must be a whole disk, must not be the
disk the system is running from, must have nothing mounted — and the
confirmation requires typing the device's name.

```sh
sudo boot2deb-install-to /dev/mmcblk0
```

The pressed card is a derived copy that *carries* the artifact; the embedded
image is the artifact itself, byte for byte. `--embed-image` needs a
combined-layout recipe built with compression on (the default).

## A split build

```sh
boot2deb press turing-rk1/forky --layout split \
    --boot-out emmc-boot.img --rootfs-out nvme.img
```

The boot image goes to the medium the board boots from (eMMC or SPI), the
rootfs image to whatever disk the OS lives on. The seed — and any addition —
rides with the rootfs, so personalization lands on the disk the OS reads; the
boot image is streamed unchanged.
