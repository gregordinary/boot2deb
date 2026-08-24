# Config model

A build is a single point across the axes a user selects:

**device × kernel × u-boot × suite × layout, plus composable features**

- **device** — the target hardware. It resolves through a layered hardware stack (see
  below).
- **kernel** — an orthogonal axis that owns everything version-coupled: its source
  refs, `.config` fragments, and [patch series](#patch-series-belong-to-the-kernel).
  A device declares which kernels it supports and a default; override with `--kernel`
  (values from `list-kernels`). Some kernels are [not built at
  all](#kernels-are-compiled-or-installed).
- **suite** — the Debian suite (e.g. `forky`, `trixie`); override with `--suite`. The
  image's `sources.list` carries the pockets that suite actually publishes, so a
  released suite gets `-security` and `-updates` alongside its base and `sid` gets
  neither.

  Like the kernel, the suite is a **closed set per board**: a device declares
  `supported_suites` and a `default_suite`, and anything else is a resolve-time error
  naming the valid list. A suite is a claim about the board as much as about Debian —
  the DT, the firmware, and the driver its Wi-Fi part needs all have to exist in that
  suite's kernel — so an RK3576 board on `bookworm` is caught at resolve rather than
  minutes into a bootstrap. A board whose config is genuinely suite-agnostic declares
  `supported_suites = ["*"]`, which is the whole list or none of it: mixing the wildcard
  with named codenames states two different claims and is rejected.
- **u-boot** — the bootloader's own axis, off the kernel entirely: a device declares
  `supported_uboot_series` and a default, and a recipe or `--uboot-series` picks one.
  Selecting a series applies its `uboot`-scope patches over the compiled u-boot and
  leaves the kernel tree untouched, so a bootloader variant costs a series rather than a
  whole kernel definition. See
  [The bootloader is its own axis](#the-bootloader-is-its-own-axis). Empty on a board
  whose u-boot ships pristine, or whose firmware is not ours to build.
- **layout** — how the disk image is packaged: `combined` (one whole-disk image, boot
  payload and rootfs on a single medium) or `split` (separate bootloader and rootfs
  images for a two-medium install); override with `--layout`. Only a boot method that
  *has* a bootloader can split it off; the combination is rejected at resolve for one
  that does not.
- **features** — a *list* of composable add-ins stacked onto the base image:
  a **capability** feature that provides a hardware stack (`media-accel-rockchip`, the
  RK35xx HW-transcode userspace) or an **application** feature that installs an app
  (`jellyfin`). A capability feature reaches the kernel as well as the rootfs — see
  [A feature can reach the kernel](#a-feature-can-reach-the-kernel). Features are the
  knob the RK1 recipes differ by over one shared device
  and kernel: `turing-rk1/forky` (base) selects none, `turing-rk1/media-accel-forky` adds
  the capability, and `turing-rk1/jellyfin` adds the app on top of it. Override with
  `--feature` (repeatable; values from `list-features`).

Two more knobs round out a build without being headline axes: `--boot-method` (a device
property, rarely overridden) and `--image-size`. The depthcharge **board profile** is a
third — see [Board profiles](#board-profiles) — and, like the localization axes, it is
resolved from config rather than set at build time.

The system locale, timezone, and console keymap are resolved the same way, and are split
across two layers for a reason: see
[Locale, timezone, and keyboard](../localization.md).

## Kernels are compiled, or installed

A kernel definition's `flavor` decides what shape it has, because the two kinds of
kernel have almost nothing in common:

- **`mainline` / `vendor`** — compiled from source. The definition owns a source ref, a
  base defconfig, a fragment list, and a patch series, and the build clones the tree,
  applies the series, merges the config, and runs `make bindeb-pkg`. The lock pins the
  exact commit.

- **`distro-package`** — installed from the Debian mirror. The definition owns nothing
  but a package name (`linux-image-armmp`); Debian owns the source, the config, and the
  patches. There is no compile node, no fragment merge, no patch series, and **no
  `[kernel]` table in the lock** — the exact version and hash are pinned in the solved
  package manifest, alongside every other package in the image.

This is not a shortcut. For a board whose SoC and device tree are fully upstream —
every Veyron Chromebook, for instance — compiling a kernel would add a cross-build and
a maintenance burden to arrive at a *worse* version of what `apt` already ships: one
that stops receiving Debian's security updates on the running board. Where Debian's
kernel runs the hardware, using it is the right answer, and the model says so rather
than pretending otherwise.

One definition then serves every suite, because the suite picks the version:
`asus-c201/forky` and `asus-c201/trixie` name the same `debian-armmp` kernel and resolve
7.1.x and 6.12.x respectively.

A distro kernel rejects the two device fields it could never act on — `device_dts` and
`device_config_fragments` are compile inputs, and a board that declared them with a
kernel that compiles nothing would read as configured and boot as broken.

## Boot methods describe different things

A boot method is not a set of options on a common shape — the shapes genuinely differ —
so `boot-methods/<method>.toml` is a **variant per method**, and the file's own name
selects it. A field belonging to another method is an *unknown field*: a parse error
naming the file, not a value quietly carried into a build with nowhere to put it.

- **`rockchip-rkbin`** — we compile the bootloader. The layer carries the u-boot source
  and ref and the raw-gap offsets (`idbloader_offset`, `uboot_itb_offset`,
  `rootfs_offset`); the device carries `uboot_defconfig` and inherits an rkbin blob set
  (ATF + DDR TPL) from its SoC. The payloads land *outside* any partition, in the gap
  ahead of the rootfs.

- **`depthcharge`** — we compile no bootloader at all. The firmware is the board's own
  (coreboot in an SPI chip), and what it loads is the **kernel itself**, vboot-signed and
  wrapped in a FIT, from a *ChromeOS kernel partition* it finds by scanning each medium's
  GPT for a type GUID. The layer carries those partitions' geometry and the GPT attribute
  bits that make the firmware boot one of them (`priority` / `tries` / `successful`), plus
  the command line to bake into the signature. The device carries a **board profile**.

  `kpart_slots` is the field worth understanding. It is how many kernel partitions the
  image lays down, back to back, and it is **2**: the first carries the signed kernel, the
  second ships empty at priority 0. That spare is what makes an on-device kernel upgrade
  atomic — the upgrade writes the slot the board is *not* booted from, so a kernel that
  fails to come up leaves the previous one intact for the firmware to fall back to. At one
  slot there is no fallback and a bad upgrade needs external media to recover. See
  [Upgrading the kernel](../kernel-upgrades.md).

Because the requirements are method-scoped, a board is only ever asked for fields its
own boot method reads: the C201 declares no `uboot_defconfig` and no rkbin blobs, and
omitting them is not an error — omitting its `[depthcharge]` block is.

### Board profiles

A depthcharge **board profile** is `depthcharge-tools`' codename for a *firmware
behaviour set* — its payload ceiling, and whether it loads a FIT ramdisk or needs the
initramfs address patched into every DTB. It describes **the firmware a unit runs**, not
the board model: the same C201 takes one profile on stock firmware and another with
libreboot installed. So the device declares a default and the profiles it supports, and
a recipe's `board` (or `resolve --board`) selects among them.

The default is deliberately the *stock* profile: a stock-profile image boots on stock
firmware **and** on a unit running libreboot, while the reverse is not true.

The profile decides what goes into the signed kernel partition, so — like the locale and
the keymap — it is config the image is resolved *from*, not a flag applied to a finished
lock. `build` therefore takes no `--board`: selecting a non-default profile means a
recipe that pins it. `resolve --board` previews one and names the file to write.

A profile also bounds the payload its firmware will accept, and a bound the *partition*
cannot hold buys nothing — so a device built for a wider profile states the matching
`kpart_size` in its own `[depthcharge]` block, and resolution derives the rootfs offset
from `kpart_offset + slots x kpart_size` so the partitions cannot disagree.
`devices/asus-c201-libreboot.toml` is that pairing: it `extends = "asus-c201"` and states
only `board = "speedy-libreboot"` and `kpart_size = "32MiB"`.

## Patch series belong to the kernel

A **patch series** (e.g. `rk3588-accel`) is the ordered patch series applied to the
source trees before they compile. It is **a property of the kernel definition, not a
user-selected axis**: a kernel names its series via `patch_series` in
`kernels/<id>.toml`, and there is deliberately no `--series` flag, because a series
that applies to one kernel version does not apply to another — so the series is
version-coupled to the kernel that owns it. Series live in a separate `patches` repo,
not in this one. Authoring workflow:
[Adding a patch](../contributing/adding-a-patch.md).

The lock's `[patches]` block records the series plus the same three fields every other
pinned source carries — where it came from, the ref that was resolved, and the exact
commit:

```toml
[patches]
series = "rk3588-accel"
source  = "https://github.com/gregordinary/patches.git"
ref     = "main"
commit  = "527d03d54ea68a375b814ccb3314901530cb8b32"
```

The commit is the reproducibility pin; the ref is the human-legible half, so "this image
used patches v1.3.0" reads without decoding a SHA. Until the series has a release tag,
`main` is the honest value — it says the pin came from the tip of development rather
than implying a release nobody cut.

`source` earns its place independently of tags. `verify-sources` grades every pin's
durability, and this axis needs it most: `update` takes the patches commit from a local
checkout's `HEAD` rather than resolving a remote ref, so it is the pin likeliest to name
something that exists nowhere else — a series committed locally and not yet pushed pins
fine and then fails for everyone. A kernel definition that names a `patch_series` must
therefore also name a `patches_url`; resolution rejects one without the other.

### Two ranges, not one

A series declares an overall `applies_to_kernel` envelope, and each entry in a scope
list may narrow itself further inside it:

```toml
applies_to_kernel = ">=7.0, <7.4"      # the envelope

kernel = [
  "media-accel/kernel/040-vdpu381-multicore-v1-curated.patch",             # no range = always
  { path = "media-accel/kernel/050-av1-iommu-v14.patch", kernels = "<7.2" },
  { path = "media-accel/kernel/050-av1-iommu-v15.patch", kernels = ">=7.2" },  # reworked at 7.2
  { path = "rocket/084-rocket-drv-fix-bo-mm-uaf.patch", kernels = "<7.3" },    # upstreamed in 7.3
]
```

The envelope gates the build; the per-entry ranges select which patches that build
actually applies. Both are *declared intent* — the `git am` pass is the enforcement.

`applies_to_kernel` governs the kernel-family scopes (`kernel`, `ffmpeg`, `userspace`).
The `uboot` scope has its own envelope, `applies_to_uboot`, matched against the pinned
u-boot tag — the two axes move independently, so a series that patches both makes a
separate claim about each. u-boot's zero-padded `vYYYY.MM` tags are accepted on both
sides of a range, so `applies_to_uboot = ">=2026.01, <2027.01"` reads the way the tags
do. A scope whose envelope is omitted claims every version, which is the shape every
shipped u-boot series takes: each is written for the one u-boot generation its board
runs.

This shape exists because the patch series changes discontinuously while kernels move
continuously. A kernel bump where everything still applies changes nothing here except
the envelope: no copied lists, no forked series. When one patch does break, the
boundary is expressed on that patch alone, and the version-insensitive majority stay
bare strings. Upstreaming gets a first-class encoding too — an upper bound reading
"needed until mainline absorbed it."

Because both alternatives live in one list, a single repo checkout still builds 7.1 and
7.2 correctly; a flat list mutated in place would lose that.

Fork a **new series name** only when the series *shape* diverges enough that one list
is confusing. Series names stay semantic, never version-suffixed, so the kernel
definitions referencing them stay stable.

An entry whose range no longer overlaps the envelope is unreachable by construction —
no kernel the series admits can select it. That is mechanically decidable rather than a
judgement call, so it is reported as a lint rather than left to a cleanup someone has to
remember. Retiring such an entry, file included, is safe: an old lock names an old
`patches` commit whose tree still contains both.

A kernel may apply **no series at all** — a stock mainline kernel whose SoC is fully
upstream, or a vendor tree that already ships its patches. It writes
`patch_series = "none"`, and then the build never reads the `patches` repo: nothing is
fetched, nothing is applied, `verify-patches` reports there is nothing to verify (on a
recipe whose u-boot axis is also bare), and
the lock **omits its `[patches]` block entirely** rather than pinning a commit the build
never consumes. Such a board builds on a machine with no `patches` checkout.

## The bootloader is its own axis

A board's u-boot is not one thing. The same silicon support can be packaged as a
minimal image that only flashes the board over USB, as the bootloader an OS image
ships with, or as a recovery tool with a boot menu and diagnostics. Those differ by a
patch series over the same u-boot tag — so the bootloader gets **its own axis**, sitting
beside the kernel rather than under it:

```toml
# devices/<board>.toml
supported_uboot_series = ["rk3576-display", "h96-max-m9-util"]
default_uboot_series    = "rk3576-display"
```

A series is a patch series in the same `patches` repo the kernel series lives in,
selected per recipe (`uboot_series = "..."`) or per invocation (`--uboot-series`),
and validated against the device's supported set exactly as the kernel axis is. The
repo it is fetched from is the boot method's `patches_url`/`patches_ref`, and the
resolved commit lands in the lock's `[uboot_patches]` block — a full pin like every
other fetched source, graded by `verify-sources` and recorded in each image's
provenance manifest.

Everything the kernel axis gets, this axis gets: `verify-patches` dry-runs the series
against the pinned u-boot, `patch import` names the recipes an import into it
invalidates, and the series' `applies_to_uboot` envelope gates the build the way
`applies_to_kernel` gates the kernel one.

A board whose u-boot ships pristine simply declares no series — the RK1 does — or,
if it lists some and wants none for this build, selects `"none"`, the same sentinel
the kernel axis spells as `patch_series = "none"`. Either way the build fetches
nothing and the lock omits `[uboot_patches]` entirely. Declaring series but no
default, with none selected, is a config error rather than a silent fallback to
pristine.

### A recipe whose deliverable is the bootloader

Because the axis is independent, a recipe can name a bootloader and *nothing else*:

```toml
# recipes/rk3576-generic/util.toml
device        = "rk3576-generic"
deliverable   = "uboot"
uboot_series = "rk3576-util"
```

`deliverable = "uboot"` means the artifact is the bootloader alone. Such a build
resolves no kernel, no suite, no features, and no rootfs, and its lock records only the
u-boot pins. Setting a rootfs axis on one — `--suite`, `--feature`, `--image-size`, a
locale — is an **error**, not a value quietly dropped: there is nothing for it to
change, and accepting it would be indistinguishable from acting on it.

The deliverable only exists where the boot method builds a bootloader of ours. A
depthcharge board's firmware is its own, so `deliverable = "uboot"` on one is rejected
at resolution.

A device may exist purely to home such recipes. `rk3576-generic` is not a board: the
SoC-generic u-boot images build from a control DTB that is identical on every RK3576
board, so they live on a tool host rather than being duplicated per board. See
[RK3576 u-boot images](rk3576-uboot-images.md) for the worked example.

## Out-of-tree modules are their own layer

Some hardware is driven by a module that lives in nobody's kernel tree — a Wi-Fi part
whose vendor maintains its own repo, say. That is not a patch series: it is a *fifth*
source tree, fetched from a third-party repo at a commit boot2deb pins. It gets its own
config layer, `kmods/<name>.toml`:

```toml
description = "AICSemi AIC8800 SDIO Wi-Fi (radxa-pkg tracking fork)"

git    = "https://github.com/radxa-pkg/aic8800.git"
ref    = "main"
subdir = "src/SDIO/driver_fw/driver/aic8800"

repo_patches  = ["fix-sdio-firmware-path.patch"]   # the fetched repo's own quilt
local_patches = ["0001-sdio-linux-7.1.patch"]      # ours, kmods/aic8800/patches/
make_args     = ["CONFIG_FDRV_NO_REG_SDIO=y"]
modules       = ["aic8800_bsp", "aic8800_fdrv"]
```

A board opts in by **name only**:

```toml
device_kmods = ["aic8800"]
```

The build fetches the repo at the locked commit, applies `repo_patches` then
`local_patches` (both `git apply -p1` unified diffs, not a `git am` series), builds the
modules against that board's freshly compiled kernel with `make M=<subdir>`, and ships
them as `<name>-modules-<kver>`. Firmware named in the layer becomes a separate
`Architecture: all` `<name>-firmware` deb, so two coexisting kernels never collide over
one firmware path. `boot2deb list-kmods` prints what is available.

**Why not the `patches` repo.** That repo is scoped to the four trees boot2deb pins
itself — kernel, u-boot, ffmpeg, userspace — and its series carry kernel-version
envelopes, because a kernel patch's applicability is keyed to a kernel version. A kmod's
patches are keyed to a *driver revision* instead, and a lock carries exactly one
`patches` pin, so routing a kmod tweak through it would couple that tweak to every
kernel, u-boot, and ffmpeg series pinned in the same lock.

**No per-board overrides.** A device names a kmod; it cannot retune one. The deb is
`<name>-modules-<kver>` and the artifact-cache node is `kmod:<name>`, and a local patch
does not move the upstream commit the version is built from — so two boards overriding,
say, `make_args` under one name would put different content behind one key. A board that
needs different build flags authors its own `kmods/<name>.toml`; a distinct name is a
distinct cache node, correct by construction. An out-of-tree overlay can still retune a
shipped kmod (or replace one of its patch files), because a kmod merges across the
search path like every other layer.

## The hardware stack

The device's hardware properties resolve by merging four TOML layers, lowest to
highest precedence:

```
arches  ←  socs  ←  boot-methods  ←  devices
```

Each layer states only its deltas. A value lives at the lowest layer that fully
determines it — for example, the DDR TPL blob is board-memory-specific, so it lives at
the **device** layer, not the soc layer. The kernel axis is resolved separately and
merged in, since a kernel's refs and fragments are coupled to its version rather than
to the hardware.

The config layers are the top-level directories:

```
arches/  socs/  boot-methods/  devices/  kernels/  kmods/  features/  recipes/
```

with vendored bootloader blobs under `blobs/<soc>/`, kernel `.config` fragments under
`fragments/`, each kmod's own patches under `kmods/<name>/patches/`, and the resolved
exact pins in `recipes/<device>/<leaf>.lock`.

### Media-accel sources ride the feature, not the SoC

The `[userspace]` (MPP/RGA/Mali) and `[ffmpeg]` source stanzas at the soc layer are
**optional**. They provide the trees a `requires_media_accel` feature compiles, and they
are copied into a build only when a selected feature declares it. A recipe that builds no
transcode stack carries no such sources and skips the userspace/ffmpeg compile nodes
entirely; a SoC that never transcodes omits the stanzas. Selecting a
`requires_media_accel` feature on a SoC that lacks them is a resolve-time error, so the
coupling is checked, not assumed.

Each individual tree is optional too, and an absent one is a statement about the
hardware rather than an omission. A SoC declares what it has:

| | RK3588 | RK3576 |
|---|---|---|
| `[userspace.mpp]` | yes | no — no vendor `mpp_service` in a mainline kernel |
| `[userspace.librga]` | yes | yes |
| `[userspace.libmali]` | yes — CSF GPU, no mainline driver | no — panfrost, so Mesa from the mirror |
| `[ffmpeg.rockchip]` | yes — the rkmpp/rkrga graft | no — the base tree builds unmodified |

That set is the capability statement the build reads, not just provenance: ffmpeg's
`./configure` surface is **derived** from it, so a SoC declaring no MPP is never asked
for `--enable-rkmpp` (and never build-depends on a `librockchip-mpp-dev` nothing
produces). The lock mirrors it one-for-one, omitting the table for any tree the SoC
does not declare.

### A feature can reach the kernel

A capability is often not purely userspace. A hardware-accel provider whose driver is
out-of-tree has to patch and configure the kernel for the hardware to exist at all, so
alongside its packages and overlay a feature may declare:

```toml
patch_series   = ["rk3576-rga"]      # series that add the driver to the tree
config_fragments = ["accel/rk3576-rga"]  # kconfig that compiles it
```

Both are needed together — a fragment can only turn on code the tree contains. They
compose **after** the kernel's own `patch_series`/`config_fragments` and the device's
`device_patch_series`/`device_config_fragments`, so a feature gets the last word on a
symbol the layers below it also set, matching the way its packages stack last in the
rootfs merge.

Putting them on the feature rather than the kernel layer is what keeps the opt-in and
the thing opted into in one place: an RK3576 build that did not select
`media-accel-v4l2` does not carry a large out-of-tree driver it has no consumer for.

Both fields require a **compiled** kernel. A distro-package kernel merges no kconfig and
applies no series, so selecting such a feature against one is a resolve-time error naming
the feature — otherwise the capability would install its userspace against hardware
support that was never built.

### A board device tree that is not yet upstream

A device normally names an in-tree DTB with `kernel_dtb`, and the kernel's own tree
builds it. A freshly-supported SoC often has every driver upstream but none of its
boards, so a device may instead carry its device-tree **sources** in `device_dts` — the
board `.dts` plus any board-specific `.dtsi`, as config-root-relative paths resolved
along the overlay search path like a fragment or blob:

```toml
kernel_dtb = "rockchip/rk3576-h96-max-m9.dtb"
device_dts = ["devices/h96-max-m9/dts/rk3576-h96-max-m9.dts"]
```

The kernel stage copies them into `arch/<arch>/boot/dts/<dt_dir>/` after the clone and
`git am`, then teaches that directory's `Makefile` to build the DTB, so `bindeb-pkg`
ships it in the `linux-image` deb like any in-tree board — and a forked board `.dts`'s
`#include "<soc>.dtsi"` resolves for free. Each source is content-hashed into the kernel
tree's signature, so editing the `.dts` rebuilds. Resolution checks that `kernel_dtb` is
actually built by one of the listed sources, and that each entry is a contained relative
`.dts`/`.dtsi` path.

`device_dts` adds a *new* board device tree. Editing an *existing* upstream `.dts` is a
patch's job, and a source that would overwrite an in-tree file is refused. For the
edit → reflash loop, `build <recipe> --stage dtb` rebuilds just that DTB in seconds.

### Extra kernel arguments per board

A board that needs boot-time kernel parameters — a workaround for an output the kernel
cannot drive, an idle state the platform firmware mishandles — declares them once at
the device layer:

```toml
kernel_cmdline = "drm_kms_helper.fbdev_emulation=0 video=HDMI-A-1:d cpuidle.off=1"
```

The value is appended to the boot path's generated command line: the extlinux path
ships it in `/etc/boot2deb/board.conf` (as `EXTL_CMD_LINE`, which `mk_extlinux` reads on
every kernel install), the depthcharge path appends it to the boot method's signing
cmdline. Base arguments stay generated — `root=` in particular is derived from
`/etc/fstab` on the device and is rejected here, as is anything the shell would
interpret when sourcing `board.conf`. A board with no entry gets the generated command
line alone.

### Every build gates its console

Among the generated base arguments is `loglevel=4`, on every board and both boot paths:
the console shows `KERN_ERR` and worse, and everything else stays in the kernel ring
buffer where `dmesg` and `journalctl -k` still show it.

This exists because a single chatty driver can otherwise print faster than a login can
be typed, which costs you the console exactly when a first boot needs it. Out-of-tree
vendor drivers are the usual source: a bare `printk()` carries no severity, so it lands
at `KERN_WARNING` however trivial the message, and such calls are typically ungated by
any of the driver's own debug knobs — lowering a driver's debug level does not reach
them. Gating the console bounds every driver at once, including the ones nothing else
can quiet.

A board that wants a louder console appends its own `loglevel=` to `kernel_cmdline`.
Device arguments are appended after the generated ones and the kernel takes the last
value, so the board wins:

```toml
kernel_cmdline = "loglevel=7"
```

### A variant board extends another

Sometimes two devices are the same board with one difference: a block enabled for
bring-up, a different DTB, a different memory fitting. The difference is real enough to
need its own device — `device_dts` and the DTB name are device-layer fields — but
everything else is the same hardware. Such a device names its parent and states only its
deltas:

```toml
extends = "h96-max-m9"

description = "H96 MAX M9 (RK3576) TV box -- 16 GB fitting"
hostname    = "h96-max-m9-16g"
kernel_dtb  = "rockchip/rk3576-h96-max-m9-16g.dtb"
device_dts  = [
    "devices/h96-max-m9/dts/rk3576-h96-max-m9.dts",
    "devices/h96-max-m9-16g/dts/rk3576-h96-max-m9-16g.dts",
]
```

The parent is merged under the child by the same rules the overlay search path uses:
tables merge key-by-key, and **a scalar or array is replaced wholesale, not
concatenated**. So a variant that wants to add one entry to an inherited list restates
the list — which is why the example above restates the parent's `device_dts` source
alongside its own wrapper. Chains are walked to the base-most device, and a cycle is a
named error rather than a hang.

Reach for this only when the difference genuinely needs a device tree or another
device-layer field. A capability whose whole expression is packages, kernel config, and
a patch series is a [feature](#a-feature-can-reach-the-kernel) instead — features
[compose a-la-carte](#a-feature-selection-is-a-build-point-not-a-new-recipe), where a
variant device does not.

The parent's **assets come too**: its `overlay/` tree is laid into the rootfs before the
variant's, so the variant inherits the parent board's runtime config — driver tuning in
`modprobe.d`, systemd units, keymaps — and can override any file of it by shipping its
own copy at the same path. This is the half a hand-copied variant cannot express: TOML
keys can be duplicated by hand, but a device's overlay tree is found by the device's
*name*, so a variant with no tree of its own would otherwise get none at all and still
build a plausible image.

The two merge axes compose. The `extends` chain is flattened first, then the search path
merges over the result, so an out-of-tree overlay can retune the parent — and have it
reach every variant — or retune one variant alone.

### Explicit over derived

Several device values are redundant with a value the resolver could derive:
`default_kernel` must also appear in `supported_kernels`; `boot_method` in
`supported_boot_methods`; `kernel_dtb` repeats the SoC's `dt_dir` prefix; `default_suite`
appears on both the device and any recipe that pins it. These are kept **explicit on
purpose**: every value a board contributes is visible in its own file and greppable
across the tree, which matters more in a small hand-authored config repo than saving a
few lines. The redundancy is not unchecked — resolution rejects a `default_kernel` outside
`supported_kernels`, a `boot_method` outside `supported_boot_methods`, and so on — so a
drifted duplicate fails fast rather than silently. `boot2deb new-device` emits these
values for you, so the boilerplate is paid by the generator, not the author.

## Recipes and the lock

A **recipe** (`recipes/<device>/<leaf>.toml`) pins one buildable point: it names the device
and, optionally, the kernel, suite, features, layout, and image size (each omitted axis
falls back to the device default). Its **lock** (`recipes/<device>/<leaf>.lock`) holds the
exact resolved pins: for every git source, the repo URL it was pinned from plus the
ref and commit, blob content hashes, and the solved rootfs manifest digest.

Recipes group under their device's folder, so a board's whole matrix — every suite and
variant — sits together; the reference you build is that path without the extension
(`turing-rk1/media-accel-forky`), the leaf dropping the device prefix the folder already
carries.

**A lock records what the build depends on, and nothing else.** Each table is present
only when the build actually has that dependency: `[kernel]` when a kernel is compiled,
`[uboot]` and `[blobs]` when a bootloader is, `[patches]` when a series is applied,
`[userspace]`/`[ffmpeg]` when the media-accel stack is. Pinning a commit nothing
consumes would record provenance for a dependency that does not exist — and would make
`update` demand a checkout the build never reads. Taken to its limit, a board that
installs Debian's kernel and boots its own firmware has a lock with exactly one table:

```toml
[rootfs]
suite = "forky"
manifest = "forky.pkgs.lock"
```

That is the whole truth about what it depends on, and the package manifest beside it
pins every one of those packages by name, version, and sha256.

The split between the two is what makes a build reproducible:

- **`update`** is the only command that consults upstream. It resolves refs to commits,
  hashes blobs, and writes the lock.
- **`build`** reads only the lock. It touches no network for its pins, so the same lock
  always produces the same inputs. Before building it checks the lock against a fresh
  resolution on every axis the lock records from config — the source repos, blob file
  names, kernel id, suite, patch series, extra debs — and refuses on drift, so a
  config edit after `update` (say a boot-method flip to a different u-boot repo) is a
  named error rather than a build against stale pins.

See the [CLI reference](cli.md) for the commands that operate on these.

### A recipe declares what it has been taken through

A recipe may carry a `[support]` claim — `validated`, `expected`, or `experimental`,
plus the `YYYY-MM-DD` the claim was last established:

```toml
[support]
status = "validated"
date   = "2026-07-16"
```

The claim is per recipe, not per device, because it varies within a device: a board can
have one build point booted and another — a different kernel, suite, or feature set —
never built. It is optional, and absent means *no claim made*, which is the honest state
for a recipe you authored against your own board. Every recipe boot2deb ships declares
one.

This is the **declared** half of the project's support story. The [support
matrix](support-matrix.md) is the generated half: it reads the pins from each recipe's
lock and sets them beside the claim, so the table cannot describe a combination the
build would not produce. The two are kept honest at the one moment they can be driven
apart — `update` warns when it moves the pins out from under a `validated` claim, since
moving them retires the evidence the claim rested on.

### A feature selection is a build point, not a new recipe

The feature axis is a list, so the number of *legal* selections grows exponentially in
the number of features — and most of them are nobody's curated point. "The shipped H96
image, plus hardware decode" is a perfectly reasonable thing to want and a poor reason
to author a file.

So a build point is a recipe **plus** a feature selection, written as a **reference**:

```text
h96-max-m9/forky                        the recipe as authored
h96-max-m9/forky+media-accel-v4l2       that recipe, with this feature selected
turing-rk1/forky+media-accel-rockchip+jellyfin
```

Everything but the features comes from the recipe, so a selection cannot drift from the
board it names. The selection *replaces* the recipe's own `features` list rather than
adding to it, which is the same thing `--feature` has always meant for `resolve`. Both
spellings work everywhere, and mean the same point:

```sh
boot2deb update h96-max-m9/forky --feature media-accel-v4l2
boot2deb build  h96-max-m9/forky+media-accel-v4l2
```

**A variant is locked like anything else.** `update` writes
`recipes/h96-max-m9/forky+media-accel-v4l2.lock` beside the recipe's own, with its own
solved package manifest, and `build` compiles it in its own work directory under a
distinct image identity — so two selections can coexist without one landing on the
other's artifacts. Every lock-reading command takes the reference, so `why-rebuild`,
`verify-patches`, `verify-sources`, and `clean` all work on a variant unchanged. A
variant's first `update` inherits the recipe's pins, so it starts from the same kernel,
u-boot, and blob commits the recipe was pinned at.

Three things follow from a variant being a build point rather than a recipe:

- **It carries no support claim.** The claim belongs to the recipe, and a different
  feature set is a different build. `list-recipes` and the support matrix show only
  authored recipes; a variant appears in neither.
- **Feature order is significant, so it is preserved.** `config_fragments` and
  `patch_series` compose in selection order, so a later feature wins a kconfig
  conflict. Two orderings of one set are two references — sorting them into one name
  would give two materially different builds a single identity.
- **A selection with no lock is an error, not an implicit `update`.** `build` reads
  locks; it never resolves one. The error names the `update` line to run.

Curate a recipe when a point is worth *claiming* — something you have booted, or intend
to support. Use a variant for everything else.

## Crates

The builder is a Rust workspace of three crates:

```
crates/core     typed model, layer resolution + validation, patch-series / lock /
                kconfig formats (pure, deterministic, unit-tested — no Linux host)
crates/engine   Linux side effects: git shell-outs, the lock resolver, the patch
                verify gate, kernel-config generation, the compile stages (kernel /
                u-boot / userspace / ffmpeg), the rootfs + image nodes, and the host
                preflight behind `doctor`
crates/cli      the boot2deb binary
```

`core` is pure and testable without a Linux host; all side effects (the filesystem,
subprocesses, the network) live in `engine`.
