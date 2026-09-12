# Config model

A build is a single point across the axes a user selects:

**device × kernel × u-boot × suite × layout, plus composable features**:

- **`device`** — the target hardware. It resolves through a layered hardware stack (see
  below).
- **`kernel`** — an orthogonal axis that owns everything version-coupled: its source
  refs, `.config` fragments, and [patch series](#patch-series-belong-to-the-kernel).
  A device declares which kernels it supports and a default. Override with `--kernel`
  (values from `list-kernels`). Some kernels are [not built at
  all](#kernels-are-compiled-or-installed).
- **`suite`** — the Debian suite (e.g. `forky`, `trixie`). Override with `--suite`. The
  image's `sources.list` carries the pockets that suite actually publishes, so a
  released suite gets `-security` and `-updates` alongside its base and `sid` gets
  neither.

  Like the kernel, the suite is a **closed set per board**. A device declares
  `supported_suites` and a `default_suite`, and anything else is a resolve-time error
  naming the valid list.

  A suite is a claim about the board as much as about Debian. The DT, the firmware, and
  the driver its Wi-Fi part needs all have to exist in that suite's kernel. So an
  RK3576 board on `bookworm` is caught at resolve rather than minutes into a bootstrap.

  A board whose config is genuinely suite-agnostic declares
  `supported_suites = ["*"]`, which is the whole list or none of it. Mixing the wildcard
  with named codenames states two different claims and is rejected.
- **u-boot** — the bootloader's own axis, off the kernel entirely: a device declares
  `supported_uboot_series` and a default, and a recipe or `--uboot-series` picks one.
  Selecting a series applies its `uboot`-scope patches over the compiled u-boot and
  leaves the kernel tree untouched. A bootloader variant therefore costs a series rather
  than a whole kernel definition. See
  [The bootloader is its own axis](#the-bootloader-is-its-own-axis). Empty on a board
  whose u-boot ships pristine, or whose firmware is not ours to build.
- **`layout`** — how the disk image is packaged: `combined` (one whole-disk image, boot
  payload and rootfs on a single medium) or `split` (separate bootloader and rootfs
  images for a two-medium install). Override with `--layout`. Only a boot method that
  *has* a bootloader can split it off, and the combination is rejected at resolve for
  one that does not.
- **`features`** — a *list* of composable add-ins stacked onto the base image. A
  **capability** feature provides a hardware stack (`media-accel-rockchip`, the RK35xx
  HW-transcode userspace). An **application** feature installs an app (`jellyfin`). A
  capability feature reaches the kernel as well as the rootfs — see
  [A feature can reach the kernel](#a-feature-can-reach-the-kernel).

  Features are the axis the RK1 recipes differ by, over one shared device and kernel.
  `turing-rk1/forky` (base) selects none, `turing-rk1/media-accel-forky` adds the
  capability, and `turing-rk1/jellyfin-forky` adds the app plus the glue that points it
  at that capability. Override with `--feature` (repeatable, with values from
  `list-features`).

Two more knobs round out a build without being headline axes: `--boot-method` (a device
property, rarely overridden) and `--image-size`. The depthcharge **board profile** is a
third — see [Board profiles](#board-profiles). Like the localization axes, it is
resolved from config rather than set at build time.

The system locale, timezone, and console keymap are resolved the same way. They are
split across two layers for a reason: see
[Locale, timezone, and keyboard](../localization.md). The NTP servers an image prefers
resolve alongside them, from `base.toml` — see
[The clock and time sync](../clock.md).

## Kernels are compiled, or installed

A kernel definition's `flavor` decides what shape it has, because the two kinds of
kernel have almost nothing in common:

- **`mainline` / `vendor`** — compiled from source. The definition owns a source ref, a
  base defconfig, a fragment list, and a patch series. The build clones the tree,
  applies the series, merges the config, and runs `make bindeb-pkg`. The lock pins the
  exact commit.

- **`distro-package`** — installed from the Debian mirror. The definition owns nothing
  but a package name (`linux-image-armmp`). Debian owns the source, the config, and the
  patches. There is no compile node, no fragment merge, no patch series, and **no
  `[kernel]` table in the lock**. The exact version and hash are pinned in the solved
  package manifest, alongside every other package in the image.

This is not a shortcut. Take a board whose SoC and device tree are fully upstream, as
every Veyron Chromebook's are. Compiling a kernel would add a cross-build and a
maintenance burden, to arrive at a *worse* version of what `apt` already ships. The
compiled one stops receiving Debian's security updates on the running board.

Where Debian's kernel runs the hardware, using it is the right answer, and the model
says so rather than pretending otherwise.

One definition then serves every suite, because the suite picks the version.
`asus-c201/forky` and `asus-c201/trixie` name the same `debian-armmp` kernel and resolve
7.1.x and 6.12.x respectively.

A distro kernel rejects the two device fields it could never act on. `device_dts` and
`device_config_fragments` are compile inputs. A board that declared them with a kernel
that compiles nothing would read as configured and boot as broken.

### A deblobbed kernel decides the whole image

A compiled kernel can declare `libre = true`, which says its source is
[GNU Linux-libre](https://www.fsfla.org/ikiwiki/selibre/linux-libre/). Every
nonfree-firmware loader has been removed from the tree, so no driver it builds can read
a blob. `kernels/rk3288-libre-7.2.toml` is the shipped one.

That is a property of the *source*, but it decides things well outside the kernel.
Resolution therefore propagates it to the whole build, rather than leaving each layer
to restate it. Three subtractions follow, and only subtractions. A build on any other
kernel resolves exactly as it would if this axis did not exist:

| What | Where it is declared | What `libre` does |
|---|---|---|
| Firmware packages | `nonfree_firmware_packages` on the SoC and device layers | left out of the merged package set |
| Vendored blobs | an `overlay-nonfree/` tree beside a SoC or device layer | not laid into the rootfs |
| Apt components | — | the image resolves and ships `main` alone, not `main contrib non-free non-free-firmware` |

Nonfree firmware is declared by the two *hardware* layers, because which blobs a build
needs is a fact about the silicon and the board. It is not a fact about the distro
substrate, the bootloader, or a userspace feature. It sits on the SoC layer when it
identifies a part every board on that SoC carries. It moves down to a device when
boards differ.

The package half is a subtraction from the *include* set rather than an entry in
`exclude`. The firmware is unreachable, not unwanted, and excluding it by name would
also forbid `apt` from pulling it in as some other package's dependency.

The cross-build sandbox deliberately keeps the full component set. It is a build
environment that is never shipped. Narrowing the toolchain a libre image is *compiled
by* would not change a byte of what is in it.

## Boot methods describe different things

A boot method is not a set of options on a common shape, because the shapes genuinely
differ. `boot-methods/<method>.toml` is therefore a **variant per method**, and the
file's own name selects it. A field belonging to another method is an *unknown field*.
That is a parse error naming the file, not a value quietly carried into a build with
nowhere to put it.

The two shipped methods:

- **`rockchip-rkbin`** — boot2deb compiles the bootloader. The layer carries the u-boot
  source and ref, and the raw-gap offsets (`idbloader_offset`, `uboot_itb_offset`,
  `rootfs_offset`). The device carries `uboot_defconfig` and inherits an rkbin blob set
  (ATF + DDR TPL) from its SoC. The payloads land *outside* any partition, in the gap
  ahead of the rootfs.

- **`depthcharge`** — boot2deb compiles no bootloader at all. The firmware is the
  board's own (coreboot in an SPI chip), and what it loads is the **kernel itself**,
  vboot-signed and wrapped in a FIT. It comes from a *ChromeOS kernel partition* the
  firmware finds by scanning each medium's GPT for a type GUID.

  The layer carries those partitions' geometry, and the GPT attribute bits that make
  the firmware boot one of them (`priority` / `tries` / `successful`). It also carries
  the command line to bake into the signature. The device carries a **board profile**.

  `kpart_slots` is the field worth understanding. It is how many kernel partitions the
  image lays down, back to back, and it is **2**. The first carries the signed kernel,
  and the second ships empty at priority 0.

  That spare is what makes an on-device kernel upgrade atomic. The upgrade writes the
  slot the board is *not* booted from. A kernel that fails to come up therefore leaves
  the previous one intact for the firmware to fall back to. At one slot there is no
  fallback, and a bad upgrade needs external media to recover. See
  [Upgrading the kernel](../kernel-upgrades.md).

Both shapes lay the same GPT: the 1 MiB `b2d-seed` partition first, then the rootfs,
which carries the **legacy-BIOS-bootable attribute**. That attribute is what points
u-boot's `bootflow scan` at the rootfs. It narrows to the partitions marked bootable,
and with none marked it looks at partition 1 alone, which is the seed. Depthcharge
ignores it and selects a kernel slot by type GUID instead.

Because the requirements are method-scoped, a board is only ever asked for fields its
own boot method reads. The C201 declares no `uboot_defconfig` and no rkbin blobs, and
omitting them is not an error. Omitting its `[depthcharge]` block is.

### Board profiles

A depthcharge **board profile** is `depthcharge-tools`' codename for a *firmware
behavior set*. That covers its payload ceiling, and whether it loads a FIT ramdisk or
needs the initramfs address patched into every DTB.

It describes **the firmware a unit runs**, not the board model. The same C201 takes one
profile on stock firmware and another with libreboot installed. So the device declares a
default and the profiles it supports, and a recipe's `board` (or `resolve --board`)
selects among them.

The default is deliberately the *stock* profile. A stock-profile image boots on stock
firmware **and** on a unit running libreboot, while the reverse is not true.

The profile decides what goes into the signed kernel partition. Like the locale and the
keymap, it is therefore config the image is resolved *from*, not a flag applied to a
finished lock. `build` takes no `--board`: selecting a non-default profile means a
recipe that pins it. `resolve --board` previews one and names the file to write.

A profile also bounds the payload its firmware will accept, and a bound the *partition*
cannot hold buys nothing. A device built for a wider profile therefore states the
matching `kpart_size` in its own `[depthcharge]` block. Resolution derives the rootfs
offset from `kpart_offset + slots x kpart_size`, so the partitions cannot disagree.
`devices/asus-c201-libreboot.toml` is that pairing. It `extends = "asus-c201"` and
states only `board = "speedy-libreboot"` and `kpart_size = "32MiB"`.

The slot size then decides more than the layout. The signed payload holds the kernel and
the initramfs in one budget, so `kpart_size` also picks the initramfs compressor. A
narrow slot takes `xz` for its size, a roomy one takes `zstd` for its speed, and
`boot2deb resolve` prints which on its `initramfs` line. It is derived rather than
authored, so widening a slot cannot leave a board paying a decompression cost it no
longer owes.

## Patch series belong to the kernel

A **patch series** (e.g. `rk3588-accel`) is the ordered patch series applied to the
source trees before they compile. It is **a property of the kernel definition, not a
user-selected axis**. A kernel names its series via `patch_series` in
`kernels/<id>.toml`, and there is deliberately no `--series` flag.

A series that applies to one kernel version does not apply to another, so the series is
version-coupled to the kernel that owns it. Series live in a separate `patches` repo,
not in this one. Authoring workflow:
[Adding a patch](../contributing/adding-a-patch.md).

The lock's `[patches]` block records the series plus the same three fields every other
pinned source carries. Those are where it came from, the ref that was resolved, and the
exact commit:

```toml
[patches]
series = "rk3588-accel"
source  = "https://github.com/gregordinary/patches.git"
ref     = "main"
commit  = "527d03d54ea68a375b814ccb3314901530cb8b32"
```

The commit is the reproducibility pin. The ref is the human-legible half, so "this image
used patches v1.3.0" reads without decoding a SHA. Until the series has a release tag,
`main` is the honest value. It says the pin came from the tip of development, rather
than implying a release nobody cut.

`source` earns its place independently of tags. `verify-sources` grades every pin's
durability, and this axis needs it most. `update` takes the patches commit from a local
checkout's `HEAD` rather than resolving a remote ref. It is therefore the pin likeliest
to name something that exists nowhere else. A series committed locally and not yet pushed pins
fine, and then fails for everyone. A kernel definition that names a `patch_series` must
therefore also name a `patches_url`, and resolution rejects one without the other.

### Two ranges, not one

A series declares an overall `applies_to_kernel` envelope, and each entry in a scope
list can narrow itself further inside it:

```toml
applies_to_kernel = ">=7.0, <7.4"      # the envelope

kernel = [
  "media-accel/kernel/040-vdpu381-multicore-v1-curated.patch",             # no range = always
  { path = "media-accel/kernel/050-av1-iommu-v14.patch", kernels = "<7.2" },
  { path = "media-accel/kernel/050-av1-iommu-v15.patch", kernels = ">=7.2" },  # reworked at 7.2
  { path = "rocket/084-rocket-drv-fix-bo-mm-uaf.patch", kernels = "<7.3" },    # upstreamed in 7.3
]
```

The envelope gates the build. The per-entry ranges select which patches that build
actually applies. Both are *declared intent*, and the `git am` pass is the enforcement.

`applies_to_kernel` governs the kernel-family scopes (`kernel`, `ffmpeg`, `userspace`).
The `uboot` scope has its own envelope, `applies_to_uboot`, matched against the pinned
u-boot tag. The two axes move independently, so a series that patches both makes a
separate claim about each. u-boot's zero-padded `vYYYY.MM` tags are accepted on both
sides of a range, so `applies_to_uboot = ">=2026.01, <2027.01"` reads the way the tags
do.

A scope whose envelope is omitted claims every version. That is the shape every shipped
u-boot series takes, since each is written for the one u-boot generation its board runs.

This shape exists because the patch series changes discontinuously while kernels move
continuously. A kernel bump where everything still applies changes nothing here except
the envelope: no copied lists, no forked series. When one patch does break, the
boundary is expressed on that patch alone, and the version-insensitive majority stay
bare strings. Upstreaming gets a first-class encoding too — an upper bound reading
"needed until mainline absorbed it."

Because both alternatives live in one list, a single repo checkout still builds 7.1 and
7.2 correctly. A flat list mutated in place would lose that.

Fork a **new series name** only when the series *shape* diverges enough that one list
is confusing. Series names stay semantic, never version-suffixed, so the kernel
definitions referencing them stay stable.

An entry whose range no longer overlaps the envelope is unreachable by construction, and
no kernel the series admits can select it. That is mechanically decidable rather than a
judgment call, so it is reported as a lint. It is not left to a cleanup someone has to
remember. Retiring such an entry, file included, is safe: an old lock names an old
`patches` commit whose tree still contains both.

A kernel can apply **no series at all**. That covers a stock mainline kernel whose SoC
is fully upstream, and a vendor tree that already ships its patches. It writes
`patch_series = "none"`, and then the build never reads the `patches` repo. Nothing is
fetched and nothing is applied. `verify-patches` reports there is nothing to verify, on
a recipe whose u-boot axis is also bare.

The lock **omits its `[patches]` block entirely**, rather than pinning a commit the
build never consumes. Such a board builds on a machine with no `patches` checkout.

## The bootloader is its own axis

A board's u-boot is not one thing. The same silicon support can be packaged three ways:

- A minimal image that only flashes the board over USB.
- The bootloader an OS image ships with.
- A recovery tool with a boot menu and diagnostics.

Those differ by a patch series over the same u-boot tag. The bootloader therefore gets
**its own axis**, sitting beside the kernel rather than under it:

```toml
# devices/<board>.toml
supported_uboot_series = ["rk3576-display", "h96-max-m9-util"]
default_uboot_series    = "rk3576-display"
```

A series is a patch series in the same `patches` repo the kernel series lives in. It is
selected per recipe (`uboot_series = "..."`) or per invocation (`--uboot-series`), and
validated against the device's supported set exactly as the kernel axis is.

The repo it is fetched from is the boot method's `patches_url`/`patches_ref`. The
resolved commit lands in the lock's `[uboot_patches]` block, a full pin like every
other fetched source. It is graded by `verify-sources` and recorded in each image's
provenance manifest.

Everything the kernel axis gets, this axis gets. `verify-patches` dry-runs the series
against the pinned u-boot, and `patch import` names the recipes an import into it
invalidates. The series' `applies_to_uboot` envelope gates the build the way
`applies_to_kernel` gates the kernel one.

A board whose u-boot ships pristine simply declares no series, as the RK1 does. If it
lists some and wants none for this build, it selects `"none"`, the same sentinel the
kernel axis spells as `patch_series = "none"`. Either way the build fetches nothing and
the lock omits `[uboot_patches]` entirely. Declaring series but no default, with none
selected, is a config error rather than a silent fallback to pristine.

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
u-boot pins.

Setting a rootfs axis on one is an **error**, not a value quietly dropped. That covers
`--suite` and `--feature`, as well as `--image-size` and a locale. There is nothing for
it to change, and accepting it would be indistinguishable from acting on it.

The deliverable only exists where the boot method builds a bootloader boot2deb makes. A
depthcharge board's firmware is its own, so `deliverable = "uboot"` on one is rejected
at resolution.

A device can exist purely to home such recipes. `rk3576-generic` is not a board. The
SoC-generic u-boot images build from a control DTB that is identical on every RK3576
board. They therefore live on a tool host rather than being duplicated per board. See
[RK3576 u-boot images](rk3576-uboot-images.md) for the worked example.

## Out-of-tree modules are their own layer

Some hardware is driven by a module that lives in nobody's kernel tree, such as a Wi-Fi
part whose vendor maintains its own repo. That is not a patch series. It is a *fifth*
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

The build fetches the repo at the locked commit, then applies `repo_patches` and
`local_patches` (both `git apply -p1` unified diffs, not a `git am` series). It builds
the modules against that board's freshly compiled kernel with `make M=<subdir>`, and
ships them as `<name>-modules-<kver>`.

Firmware named in the layer becomes a separate `Architecture: all` `<name>-firmware`
deb, so two coexisting kernels never collide over one firmware path. `boot2deb
list-kmods` prints what is available.

**Why not the `patches` repo.** That repo is scoped to the four trees boot2deb pins
itself, which are the kernel, u-boot, ffmpeg and userspace trees. Its series carry
kernel-version
envelopes, because a kernel patch's applicability is keyed to a kernel version.

A kmod's patches are keyed to a *driver revision* instead, and a lock carries exactly
one `patches` pin. Routing a kmod tweak through it would couple that tweak to every
kernel, u-boot, and ffmpeg series pinned in the same lock.

**No per-board overrides.** A device names a kmod, and cannot retune one. The deb is
`<name>-modules-<kver>` and the artifact-cache node is `kmod:<name>`, and a local patch
does not move the upstream commit the version is built from. Two boards overriding, say,
`make_args` under one name would therefore put different content behind one key.

A board that needs different build flags authors its own `kmods/<name>.toml`, and a
distinct name is a distinct cache node, correct by construction. An out-of-tree overlay
can still retune a shipped kmod (or replace one of its patch files), because a kmod
merges across the search path like every other layer.

## The hardware stack

The device's hardware properties resolve by merging four TOML layers, lowest to
highest precedence:

```
arches  ←  socs  ←  boot-methods  ←  devices
```

Each layer states only its deltas. A value lives at the lowest layer that fully
determines it. The DDR TPL blob, for example, is board-memory-specific, so it lives at
the **device** layer rather than the soc layer. The kernel axis is resolved separately and
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

The `[[userspace]]` entries and the `[ffmpeg]` source stanza at the soc layer are
**optional**. They provide the trees a `requires_media_accel` feature compiles, and they
are copied into a build only when a selected feature declares it. A recipe that builds no
transcode stack carries no such sources and skips the userspace/ffmpeg compile nodes
entirely. A SoC that never transcodes declares neither. Selecting a
`requires_media_accel` feature on a SoC that lacks them is a resolve-time error, so the
coupling is checked, not assumed.

**A tree is a value, not a field.** `[[userspace]]` is an array, one entry per tree the
part has, because *which* trees exist is the SoC's statement about its own hardware. A
fourth tree, or a different family's stack entirely, is therefore a file edit rather
than a schema change. The build stage, the lock, the plan nodes and the CLI all loop
over whatever is declared. This is the shape `kmods/<name>.toml` already has for
out-of-tree drivers, and it is there for the same reason.

An absent tree is a statement about the hardware rather than an omission:

| | RK3588 | RK3576 |
|---|---|---|
| `mpp` | yes | no — no vendor `mpp_service` in a mainline kernel |
| `librga` | yes | yes |
| `libmali` | yes — CSF GPU, no mainline driver | no — panfrost, so Mesa from the mirror |
| `[ffmpeg.rockchip]` | yes — the rkmpp/rkrga graft | no — the base tree builds unmodified |

Each entry says what the tree *is* and how the rest of the build relates to it, rather
than leaving those as rules in code:

```toml
[[userspace]]
name            = "librga"
git             = "https://github.com/tsukumijima/librga-rockchip.git"
ref             = "master"
debs            = ["librga2", "librga-dev"]   # what its packaging produces
links           = ["librga2", "librga-dev"]   # what a consumer links against
ffmpeg_flag     = "--enable-rkrga"            # the ./configure flag it earns
ffmpeg_requires = ["mpp"]                     # and what that flag needs alongside it
```

Three more keys shape a tree that needs them:

- `patched = true` marks the one tree that takes the series' `userspace` scope (the MPP
  CMA fix).
- `optional = true` means it is skipped unless a build names it with
  `--userspace <name>`.
- `build_deps` and `targets_filter` carry the extra development packages a tree's probes
  need, and a filter over a vendor variant matrix.

That set is the capability statement the build reads, not just provenance. ffmpeg's
`./configure` surface is **derived** from it, so a SoC declaring no MPP is never asked
for `--enable-rkmpp` (and never build-depends on a `librockchip-mpp-dev` nothing
produces).

`ffmpeg_requires` is what makes the rkrga rule data rather than a special case. Its
filters allocate RKMPP frames, and ffmpeg's own `./configure` rejects `--enable-rkrga`
without `--enable-rkmpp`. On the RK3576 `librga2` therefore still ships for programs
that speak the API directly, and the produced ffmpeg carries no librga `NEEDED` entry at
all. The lock mirrors the declared set one-for-one, as `[[userspace]]` entries keyed by
`name`.

### A feature can reach the kernel

A capability is often not purely userspace. A hardware-accel provider whose driver is
out-of-tree has to patch and configure the kernel for the hardware to exist at all.
Alongside its packages and overlay, a feature can therefore declare:

```toml
patch_series   = ["rk3576-rga"]      # series that add the driver to the tree
config_fragments = ["accel/rk3576-rga"]  # kconfig that compiles it
```

Both are needed together, since a fragment can only turn on code the tree contains. They
compose **after** the kernel's own `patch_series`/`config_fragments` and the device's
`device_patch_series`/`device_config_fragments`. A feature therefore gets the last word
on a symbol the layers below it also set. That matches the way its packages stack last
in the rootfs merge.

Putting them on the feature rather than the kernel layer is what keeps the opt-in and
the thing opted into in one place. An RK3576 build that did not select
`media-accel-v4l2` does not carry a large out-of-tree driver it has no consumer for.

Both fields require a **compiled** kernel. A distro-package kernel merges no kconfig and
applies no series, so selecting such a feature against one is a resolve-time error naming
the feature. Otherwise the capability would install its userspace against hardware
support that was never built.

### A feature can require another, by capability

Two features can be individually valid and useless together, or useless apart. The
model has both gates, and they are opposites:

```toml
# features/media-accel-rockchip.toml — a provider
conflicts = ["media-accel-v4l2"]     # these two cannot coexist
provides  = ["ffmpeg"]               # what this supplies to the selection

# features/jellyfin.toml — a consumer
requires_capability = ["ffmpeg"]     # something in the selection must supply it
```

`conflicts` is symmetric — declaring it on either side is enough — and rejects a
selection holding both. `requires_capability` rejects a selection holding a consumer
and no provider:

```console
$ boot2deb resolve turing-rk1/forky+jellyfin
error: feature 'jellyfin' requires capability 'ffmpeg', which no selected feature
       provides — add one of 'media-accel-rockchip', 'media-accel-v4l2'
```

That composition would otherwise build a perfectly good image whose Jellyfin exits at
startup. The feature installs no FFmpeg, and the application treats a missing encoder as
fatal. It is the cheapest class of error to catch at resolve, since nothing
about the failure is visible until the board boots.

**A capability is a free-form name, not a feature name**, and that is the whole point.
Both `media-accel-rockchip` (RK3588) and `media-accel-v4l2` (RK3576) declare
`provides = ["ffmpeg"]`. `jellyfin` therefore composes with whichever matches the SoC
while naming neither. A provider for a future platform satisfies it with no edit to the
consumer.

Names are matched literally. A misspelling on either side surfaces as this same error,
which reports when no feature in the tree provides the capability at all.

`list-features` shows both sides, which is where a rejected composition sends you:

```console
$ boot2deb list-features
ffmpeg-nonfree        soc=any     arch=any    needs=ffmpeg
jellyfin              soc=any     arch=arm64  needs=ffmpeg
media-accel-rockchip  soc=rk3588  arch=any    conflicts=media-accel-v4l2 provides=ffmpeg
media-accel-v4l2      soc=rk3576  arch=any    conflicts=media-accel-rockchip provides=ffmpeg
```

This **validates** a composition, and does not complete one. Nothing is added to the
selection to satisfy a requirement, and the recipe still names every feature explicitly.
Provider auto-resolution stays a non-goal: the builder tells you the composition is
incomplete and which features would complete it, and you choose.

### The FFmpeg a build ships is redistributable

Every recipe here builds FFmpeg with `--enable-gpl --enable-version3` and nothing that
forfeits redistribution, so the `ffmpeg-rk` `.deb` and any image holding it can be
passed on.

The other flavor is available, and is a feature rather than a flag:

```console
$ boot2deb build turing-rk1/media-accel-forky                            # free
$ boot2deb build turing-rk1/forky+media-accel-rockchip+ffmpeg-nonfree    # nonfree
```

`ffmpeg-nonfree` installs no packages. It sets one axis on the build: FFmpeg's
`--enable-nonfree`, which admits encoders whose license terms cannot be combined with
the GPL. FDK-AAC is the one this tree has a use for.

`requires_capability = ["ffmpeg"]` makes selecting it without a provider the
resolve-time error above, rather than a flag nobody reads. Note that the second form
names the *whole* selection. A `+` suffix replaces a recipe's feature list rather than
adding to it. The nonfree variant of a recipe with features of its own is therefore
spelled against a base recipe that has none.

The two are separate builds all the way down. The flavor moves the `./configure` flags
and the ffmpeg stage's build root together, and the artifact cache keys on both. Neither
flavor can therefore ever be served from the other's cache, and each variant reference
gets its own lock, work directory and artifact path. What a finished image was built
with is recorded in its provenance manifest, under `[image] features`, alongside every
other axis of the build point.

Because a `[support]` claim says a configuration is fit to publish, a recipe that both
declares one and selects `ffmpeg-nonfree` is rejected at resolution. Reach the flavor
as a variant reference. A variant carries no claim and appears in no support matrix.

Nothing on the hardware path depends on the choice. Audio is CPU work on these boards
either way. FDK-AAC's advantages are bitrates below about 96 kbps and the HE-AAC
profiles, which sit outside the 128-384 kbps range a media server transcodes to.
FFmpeg's own native `aac` encoder covers that range.

### A library FFmpeg links that Debian does not carry

Some codecs have no Debian package. AVS2 and AVS3 are the case here. FFmpeg wraps
`libdavs2` and `libuavs3d`, and the archive ships neither. `./configure` therefore has
nothing to find, and the codecs are absent from every build by default.

The `avs-decode` feature supplies them as **pinned bytes**. Its `[[extra_debs]]` entries
name `.deb` files in `debs/` and pin each by sha256, and its `[[ffmpeg_libs]]` entries
say what to do with them:

```toml
[[ffmpeg_libs]]
flag  = "--enable-libdavs2"
links = ["libdavs2-16", "libdavs2-dev"]
```

`links` is the runtime library then its `-dev`, the same contract a compiled userspace
tree carries, and both halves matter. The `-dev` alone gives `./configure` headers and a
`.pc` file, with no library to link against and no `shlibs` for `dpkg-shlibdeps` to
read. The produced `ffmpeg-rk` would then declare no dependency on a library it links.

The first entry is what the deb must end up depending on, and the stage fails rather
than shipping one where that dependency was dropped.

An `extra_debs` entry says where its bytes are wanted:

```toml
[[extra_debs]]
path    = "debs/libdavs2-16_....deb"
sha256  = "18c079694fd0..."
targets = ["image", "ffmpeg"]      # default is ["image"]
```

`image` is the local apt repo the rootfs solves against. `ffmpeg` is the ffmpeg stage's
build pool, which its build root resolves against beside the suite mirrors.

A runtime library needs both, since the build links it and the image runs it. A `-dev`
package names `ffmpeg` alone, since headers on a device compile nothing. Nothing
installs these by name. `ffmpeg-rk`'s own `Depends` does, once the bytes are in the repo
for the solve to find. That is what makes the image's apt consistent rather than
force-installed.

The pool is a real repository, so a package is resolved with its dependencies rather
than unpacked into the build root behind the resolver's back. That is the same treatment
the build's own `.deb`s get.

The pins also reach the artifact cache. The ffmpeg stage's output key folds the hashes
of the debs targeting it. A re-cut library therefore rebuilds FFmpeg, instead of
restoring one built against the old bytes.

### A board device tree that is not yet upstream

A device normally names an in-tree DTB with `kernel_dtb`, and the kernel's own tree
builds it. A freshly-supported SoC often has every driver upstream but none of its
boards. A device can instead carry its device-tree **sources** in `device_dts`. Those
are the board `.dts` plus any board-specific `.dtsi`, as config-root-relative paths
resolved along the overlay search path like a fragment or blob:

```toml
kernel_dtb = "rockchip/rk3576-h96-max-m9.dtb"
device_dts = ["devices/h96-max-m9/dts/rk3576-h96-max-m9.dts"]
```

The kernel stage copies them into `arch/<arch>/boot/dts/<dt_dir>/` after the clone and
`git am`. It then teaches that directory's `Makefile` to build the DTB, so `bindeb-pkg`
ships it in the `linux-image` deb like any in-tree board. A forked board `.dts`'s
`#include "<soc>.dtsi"` resolves for free.

Each source is content-hashed into the kernel tree's signature, so editing the `.dts`
rebuilds. Resolution checks that `kernel_dtb` is actually built by one of the listed
sources, and that each entry is a contained relative `.dts`/`.dtsi` path.

`device_dts` adds a *new* board device tree. Editing an *existing* upstream `.dts` is a
patch's job, and a source that would overwrite an in-tree file is refused. For the
edit → reflash loop, `build <recipe> --stage dtb` rebuilds just that DTB in seconds.

### Extra kernel arguments per board

A board sometimes needs boot-time kernel parameters. Examples are a workaround for an
output the kernel cannot drive, or an idle state the platform firmware mishandles. It
declares them once at the device layer:

```toml
kernel_cmdline = "drm_kms_helper.fbdev_emulation=0 video=HDMI-A-1:d cpuidle.off=1"
```

The value is appended to the boot path's generated command line. The extlinux path
ships it in `/etc/boot2deb/board.conf` (as `EXTL_CMD_LINE`, which `mk_extlinux` reads on
every kernel install). The depthcharge path appends it to the boot method's signing
cmdline.

Base arguments stay generated. `root=` in particular is derived from `/etc/fstab` on the
device and is rejected here, as is anything the shell would interpret when sourcing
`board.conf`. A board with no entry gets the generated command line alone.

### Every build gates its console

Among the generated base arguments is `loglevel=4`, on every board and both boot paths.
The console shows `KERN_ERR` and worse. Everything else stays in the kernel ring
buffer, where `dmesg` and `journalctl -k` still show it.

This exists because a single chatty driver can otherwise print faster than a login can
be typed. That costs you the console exactly when a first boot needs it.

Out-of-tree vendor drivers are the usual source. A bare `printk()` carries no severity,
so it lands at `KERN_WARNING` however trivial the message. Such calls are typically
ungated by any of the driver's own debug knobs, so lowering a driver's debug level does
not reach them. Gating the console bounds every driver at once, including the ones
nothing else can quiet.

A board that wants a louder console appends its own `loglevel=` to `kernel_cmdline`.
Device arguments are appended after the generated ones and the kernel takes the last
value, so the board wins:

```toml
kernel_cmdline = "loglevel=7"
```

### A variant board extends another

Sometimes two devices are the same board with one difference: a block enabled for
bring-up, a different DTB, or a different memory fitting. The difference is real enough
to need its own device, because `device_dts` and the DTB name are device-layer fields.
Everything else is the same hardware. Such a device names its parent and states only its
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

The parent is merged under the child by the same rules the overlay search path uses.
Tables merge key-by-key, and **a scalar or array is replaced wholesale, not
concatenated**. A variant that wants to add one entry to an inherited list therefore
restates the list. That is why the example above restates the parent's `device_dts`
source alongside its own wrapper. Chains are walked to the base-most device, and a cycle
is a named error rather than a hang.

### A package that only exists in some suites

Most packages are the same in every suite and are written as a bare name. A few are not.
Debian splits, renames and drops binary packages between releases. A layer that names
one unconditionally is therefore right on the suites it was written against, and wrong
on the rest.
An entry in any `packages` or `nonfree_firmware_packages` list can therefore name the
suites it applies to:

```toml
packages = [
    "network-manager",
    # nmtui left `network-manager` for a package of its own at 1.56.0-4, so forky and
    # sid need it named and trixie (1.52.1-1) must not — that archive has no such
    # package, and its `network-manager` already ships the binary.
    { name = "network-manager-tui", suites = ["forky", "sid"] },
    "wpasupplicant",
]
```

An entry that does not apply contributes nothing and does not reserve its name. That is
what lets a **rename** be written as two entries over disjoint suites, of which exactly
one applies on any given build:

```toml
{ name = "libv4l-0",    suites = ["bookworm"] },
{ name = "libv4l-0t64", suites = ["trixie", "forky"] },
```

The suites are **enumerated, not bounded**. A range (`since = "forky"`) would read closer
to the intent and would need no edit when a suite is added. That is the hazard rather
than the convenience, because it silently extends the claim to every future suite.
Whether a package still exists in one is a fact only that archive can answer.

Enumerating forces the claim to be restated when a suite is added, and
[`verify-packages`](cli.md) then checks each claim against the archive it is about. It
also means boot2deb owns no release sequence. There is no ordering to maintain and no
special case for `sid`.

The price of enumerating is that a misspelt suite is silent. The entry never applies, so
the package goes missing with nothing said. `verify-packages` pays it back by reporting a
`suites` name that no recipe in the tree builds:

```text
note : network-manager-tui in soc 'rk3288' names suite 'forkey', which no recipe in
       this tree builds — check the spelling
```

Debian's symbolic names (`sid`, `unstable`, `testing`, `stable`, `oldstable`) are exempt.
They are permanent fixtures of the archive, so naming one is never a typo even where no
recipe builds it yet. An entry naming an **empty** `suites` list is refused outright at
resolution. Config that can never take effect is a mistake rather than a no-op.

`exclude` takes plain names only. An include has to name a package the archive carries,
so which suite is being built decides whether the name is right. An exclude names
something that must not be installed, and is satisfied just as well by a suite that
never had it. Excluding a name a suite does not carry is therefore already a no-op.

**Five arrays are the exception and accumulate**: `caveats`, `expect`,
`nonfree_firmware_packages`, `packages` and `exclude`. Each level's entries are
concatenated base-most first and de-duplicated, so a variant inherits its parent's and
adds its own. The line is between *describing or supplying the running system* and
*selecting a build input*.

A variant is the same hardware, so it is bound by everything its parent said about that
hardware. A caveat cannot be un-said, and a runtime check that held on the parent holds
here. A radio that needed firmware still needs it, and a board package the parent
installs is one this board wants too.

Replacing any of them would let a variant that adds one entry silently drop every entry
it inherits. That is a support claim that is wrong, or a self-test that passes while
testing less than the parent's.

Everything else **selects**, and a variant makes its own selection. `device_kmods` and
`device_patch_series` choose which drivers and series the kernel is built with.
`extra_debs` pins exact bytes, since two pins of one package would be a conflict rather
than a sum. Every `supported_*` list names the alternatives a build can pick from.

A value that is not an array at any level of the chain is an error naming the file that
holds it. That includes an ancestor's, which last-wins alone would have swallowed.

Reach for this only when the difference genuinely needs a device tree or another
device-layer field. A capability whose whole expression is packages, kernel config, and
a patch series is a [feature](#a-feature-can-reach-the-kernel) instead. Features
[compose a-la-carte](#a-feature-selection-is-a-build-point-not-a-new-recipe), where a
variant device does not.

The parent's **assets come too**. Its `overlay/` tree is laid into the rootfs before the
variant's. The variant therefore inherits the parent board's runtime config: driver
tuning in `modprobe.d`, systemd units, and keymaps. It can override any file of it by
shipping its own copy at the same path.

This is the half a hand-copied variant cannot express. TOML keys can be duplicated by
hand, but a device's overlay tree is found by the device's *name*. A variant with no
tree of its own would otherwise get none at all, and still build a plausible image.

The two merge axes compose. The `extends` chain is flattened first, then the search path
merges over the result. An out-of-tree overlay can therefore retune the parent, and have
it reach every variant, or retune one variant alone.

### An image size can be stated or measured

`image_size` is normally a size — `"2G"`, `"8G"` — chosen by whoever added the board. It
is the whole-disk size of the artifact rather than of the installed system, since the
rootfs grows to fill its medium on first boot.

Picking that number is real friction on a new board, so it can also be **measured**:

```toml
image_size = "fit+20%"   # the smallest image holding the rootfs, a fifth of it free
image_size = "fit+512M"  # ... with 512 MiB free instead
```

`fit` sizes the filesystem to its contents. The size a rootfs needs is not a formula.
How much room a filesystem has left depends on several things. Those are its group
count, its inode tables, the descriptor blocks it reserves to grow into, and the journal
its size earns. Every one of them follows from the size.

The size is therefore found by *placing* the rootfs into candidate geometries, through
the format's own placement pass. The size that comes back is one that formats, and one
ext4 block less is not. The disk is then laid out around it: boot region, that
filesystem, backup GPT, nothing over.

The slack is written rather than defaulted. A fitted filesystem with nothing free is the
smallest one that holds the rootfs, which boots into a full disk. A bare `fit` is
therefore refused, and names the two forms. `fit+0%` is accepted, because an explicit
zero is a decision.

A share is capped at 90% and a byte slack at 64 GiB. Past either you have named a size,
and naming it is faster than searching for it. Both are checked by `resolve`, offline,
rather than discovered mid-build. `update` and `build` run the same gate before anything
is committed or compiled.

`fit` is opt-in per board, and nothing else changes. The shipped recipes all carry a
hand-picked size, and a stated size lays out the disk first and formats into it exactly
as before.

### Explicit over derived

Several device values are redundant with a value the resolver could derive:

- `default_kernel` must also appear in `supported_kernels`.
- `boot_method` must appear in `supported_boot_methods`.
- `kernel_dtb` repeats the SoC's `dt_dir` prefix.
- `default_suite` appears on both the device and any recipe that pins it.

These are kept **explicit on purpose**. Every value a board contributes is visible in
its own file and greppable across the tree. That matters more in a small hand-authored
config repo than saving a few lines.

The redundancy is not unchecked. Resolution rejects a `default_kernel` outside
`supported_kernels`, a `boot_method` outside `supported_boot_methods`, and so on, so a
drifted duplicate fails fast rather than silently. `boot2deb new-device` emits these
values for you, so the boilerplate is paid by the generator, not the author.

### A value that becomes a file or a line is checked at resolve

Most config values are copied verbatim into something with a grammar of its own. That
might be a file name, a shell-sourced line, an `/etc/hosts` entry, or an apt source
line. In every such case the *shape* is checked where the value is authored. A config that could
not produce a working image therefore fails `resolve`, rather than producing an image
that is quietly wrong:

| Value | Accepted shape | Because it becomes |
|---|---|---|
| device slug (`devices/<slug>.toml`) | a host name, as below | the file, the recipe folder, the overlay tree, the work dir — **and the default `hostname`** |
| `hostname` | RFC 1123 host name: `[A-Za-z0-9-]` labels joined by `.`, ≤ 63 per label and ≤ 64 total, no leading/trailing `-` | all of `/etc/hostname`, and the name half of an `/etc/hosts` line |
| `apt_sources.name` | portable file stem: `[A-Za-z0-9._-]`, not `.`/`..` | `sources.list.d/<name>.list` and `<name>.gpg` in the image |
| `apt_sources.signed_by` | the same, a bare file name | a lookup in `blobs/keyrings/` |
| `apt_sources.uri` / `suite` / `components` | non-empty, no whitespace or `[`/`]`, and the URI `http(s)` | positional fields of one apt source line |
| `kernel_cmdline` | no `root=`, nothing a shell would interpret | a line of `/etc/boot2deb/board.conf`, sourced at each kernel install |
| `depthcharge.board` | bare identifier | a key in `/etc/depthcharge-tools/config`, written through a quoted heredoc |
| `locale` / `timezone` / `keymap` | see [Locale, timezone, and keyboard](../localization.md) | `/etc/locale.gen`, the `/etc/localtime` target, shell-sourced `/etc/default/keyboard` |
| `ntp_servers` | a bare host per entry: hostname or IP, no scheme, port, or whitespace — see [The clock and time sync](../clock.md) | the space-separated `NTP=` line of a `timesyncd.conf.d` drop-in |
| `ssh_authorized_keys` | one line per entry: a known key type, a base64 blob whose own embedded type name agrees with it, an optional comment. Private key material and options prefixes are refused — see [The account, sudo, and SSH keys](../access.md) | a line of `~debian/.ssh/authorized_keys`, written through a quoted heredoc |
| `groups` | Debian's `NAME_REGEX` per entry: a lowercase letter or `_`, then lowercase letters, digits, `_` and `-`, ≤ 32 characters. A comma is refused by name, since it would split one entry into two groups — see [The account, sudo, and SSH keys](../access.md) | the comma-separated argument of one `usermod -aG` |

The rule these share is that a value is **rejected, never repaired**. A hostname with a
space in it is not trimmed, and an out-of-set source name is not folded to a legal
neighbor.

The mapping that would do the folding is not one-to-one. Two names the config states as
different repositories could therefore land on one file. The one that lost would be
missing from the finished image, with its packages already installed. Failing at the
point of authorship is the only outcome that cannot silently change what was asked for.

`apt_sources.name` doubles as the key that de-duplicates sources across features. Two
features naming the same repository collapse to one entry. The same name with
*different* settings is an error, since the solve could not tell which repo to
activate.

**A board's slug is a host name.** That is the one entry above whose rule comes from
somewhere other than the file it is written into. `hostname` defaults to the slug, and
nearly every board keeps that default, so the two are the same string in practice. A
slug outside the host-name shape would therefore make the default a value no image could
carry. Holding the slug to the tighter rule is what makes the default correct by
construction.

A board that wants a different network name simply states `hostname`
(`rk3576-evb1-v10` comes up as `rk3576-evb1`). What it cannot do is inherit an invalid
one. In practice the rule costs nothing. It rules out `_`, a leading or trailing `-`, a
doubled or trailing `.`, and names over 64 characters. The house style is already
lowercase-and-hyphens.

The alternative is letting the slug be looser and repairing the derived hostname. That
is the same trap as the source names above, and one Debian falls into on boot2deb's
behalf if it is allowed to. `hostname(5)` says systemd *filters* invalid characters out
of `/etc/hostname` rather than refusing them.

An unrepaired `my_board` would boot as `myboard`, while the `/etc/hosts` entry generated
beside it still said `my_board`. So every
lookup of the machine's own name would miss. Rejecting the slug is what keeps those two
files describing the same host.

### A value whose wrongness is invisible is bounded, not advised

The table above is about *shapes* — values that must parse as something. One value is
range-checked instead, for a different reason: `first_boot_password_length` is accepted
only between 8 and 64.

The usual argument against a hard bound is that the author knows their own situation
better than the config layer does. It does not hold here, because this is the one setting
whose effect cannot be observed anywhere. A board booted with an 8-character credential
looks exactly like one booted with a 20-character credential. Nothing on the running
system, in the image, or in a test reveals how much entropy the login had.

Every other setting announces a bad value eventually. A wrong timezone shows a wrong
clock, and a malformed key fails a login. Advice is enough for them, and a bound is what
covers this one. [The account, sudo, and SSH keys](../access.md) explains where the two
ends of the range come from.

## Recipes and the lock

A **recipe** (`recipes/<device>/<leaf>.toml`) pins one buildable point: it names the device
and, optionally, the kernel, suite, features, layout, and image size (each omitted axis
falls back to the device default). Its **lock** (`recipes/<device>/<leaf>.lock`) holds
the exact resolved pins. For every git source that is the repo URL it was pinned from,
plus the ref and commit. It also holds blob content hashes and the solved rootfs
manifest digest.

Recipes group under their device's folder, so a board's whole matrix, every suite and
variant, sits together. The reference you build is that path without the extension
(`turing-rk1/media-accel-forky`), the leaf dropping the device prefix the folder already
carries.

**A lock records what the build depends on, and nothing else.** Each table is present
only when the build actually has that dependency. `[kernel]` appears when a kernel is
compiled, and `[uboot]` and `[blobs]` when a bootloader is. `[patches]` appears when a
series is applied, and `[[userspace]]`/`[ffmpeg]` when the media-accel stack is.

Pinning a commit nothing consumes would record provenance for a dependency that does not
exist. It would also make `update` demand a checkout the build never reads. Taken to its
limit, a board that installs Debian's kernel and boots its own firmware has a lock with
exactly one table:

```toml
[rootfs]
suite = "forky"
manifest = "forky.pkgs.lock"
```

That is the whole truth about what it depends on. The package manifest beside it pins
every one of those packages by name, version, and sha256.

The split between the two is what makes a build reproducible:

- **`update`** is the only command that consults upstream. It resolves refs to commits,
  hashes blobs, and writes the lock.
- **`build`** reads only the lock. It touches no network for its pins, so the same lock
  always produces the same inputs.

  Before building, it checks the lock against a fresh resolution on every axis the lock
  records from config. Those are the source repos, blob file names, kernel id, suite,
  patch series, and extra debs. It refuses on drift, so a config edit after `update`
  (say a boot-method flip to a different u-boot repo) is a named error rather than a
  build against stale pins.

See the [CLI reference](cli.md) for the commands that operate on these.

### Re-pinning: constraints move, hand-pins stay

The config layers declare a **constraint**, such as `uboot_ref = "v2026.07"` on the boot
method or a `[[userspace]]` entry's `ref` on the SoC. The lock records the **exact pin**
resolved from it. An `update` given no per-tree ref flag re-reads the constraint, so
editing one and re-pinning carries every recipe that resolves through that layer with it:

```sh
# after bumping uboot_ref in boot-methods/rockchip-rkbin.toml
boot2deb update turing-rk1/forky
#   bumped   u-boot v2026.04 -> v2026.07 (config constraint)
```

Every pin moved this way is named in the output. A propagated bump is therefore visible
where it happens, rather than surfacing later as an unexplained ref change in a lock diff.

The exception is a lock pinned to a **bare commit sha**. No config layer authors one,
since a 40-hex ref only ever arrives through an explicit `--<tree>-ref`. It is a
deliberate hand-pin, and re-reading the constraint over it would discard that choice and
float the tree back to a branch tip. Those pins stay put until a flag moves them, which
is what lets a tree sit on a fixed commit while its constraint says `master`.

The kernel has no constraint to follow. A kernel definition declares a `track` rather
than a concrete ref, so an omitted `--kernel-ref` re-pins the previous lock's ref. Only a
first update, with no lock to inherit from, must supply one.

### A recipe declares what it has been taken through

A recipe can carry a `[support]` claim — `validated`, `expected`, or `experimental` —
plus the `YYYY-MM-DD` the claim was last established:

```toml
[support]
status = "validated"
date   = "2026-07-16"
```

The claim is per recipe, not per device, because it varies within a device. A board can
have one build point booted and another never built, differing in kernel, suite, or
feature set. It is optional, and absent means *no claim made*, which is the honest state
for a recipe you authored against your own board. Every recipe boot2deb ships declares
one.

This is the **declared** half of the project's support story. The [support
matrix](support-matrix.md) is the generated half. It reads the pins from each recipe's
lock and sets them beside the claim. The table therefore cannot describe a combination
the build would not produce.

The two are kept honest at the one moment they can be driven apart. `update` warns when
it moves the pins out from under a `validated` claim, since moving them retires the
evidence the claim rested on.

### A status says how far, not how much

`validated` means an image built from this recipe booted. It does not mean everything on
the board works, and a claim that leaves that unsaid overstates itself. What a build
point does *not* do is a `caveats` list, on the layer that owns the limitation:

```toml
# socs/rk3576.toml — true of the part, so of every board on it
caveats = [
  "HDMI tops out at 4K30 and cannot reach 4K60: the dw-hdmi-qp bridge has no SCDC/scrambling support ...",
]

# devices/h96-max-m9.toml — true of this board
caveats = [
  "No port on the box delivers USB 3.0. The blue port beside HDMI is capped to high speed ...",
]

# features/media-accel-rockchip.toml — true wherever this capability is selected
caveats = [
  "Scaling inside a hardware transcode runs on the CPU. The RGA filters accept only frames carrying an RKMPP hardware context ...",
]

# recipes/asus-c201/libre-forky.toml — true of this build point alone
[support]
status  = "expected"
date    = "2026-08-04"
caveats = [
  "The internal BCM4354 Wi-Fi and Bluetooth do not work. linux-libre removes brcmfmac's firmware request ...",
]
```

Resolution concatenates the four in that order — silicon, board, features, build point —
and de-duplicates them. A board with three recipes therefore states its limitations
once, and each recipe adds only what is its own.

Each entry keeps the layer that stated it. That is what tells a reader whether another
board would do better, or whether dropping a feature would.

A capability's limits belong to the capability, not to whichever recipe named it first.
Every recipe composing the feature inherits them, and a limit stated once cannot fall
out of step across recipes that all have it. Reserve a recipe's own `[support].caveats`
for what is true of that build point alone.

They are printed by `resolve`, at the end of a `build`, and in the
[support matrix](support-matrix.md). That matrix groups the two hardware scopes under
the board, since they hold for every recipe on it. It groups the feature and recipe ones
under each recipe, since those depend on what that recipe selected.

`caveats` **accumulates** down an `extends` chain rather than being replaced by it. It is
one of [the five arrays that do](#a-variant-board-extends-another). A variant
shares its parent's hardware, so last-wins there would let a variant that adds one
caveat silently drop every limitation it inherits. A variant cannot un-say one of its
parent's. A board that genuinely lacks the limitation is its own device rather than a
variant.

A caveat is a sentence, not a code, so the only rule at load is that it is non-empty
and carries no ragged whitespace. **Where a limitation is mechanically checkable it
belongs in the board's selftest expectations instead**, where it fails rather than
merely informs. A caveat that could have been an `[[expect]]` entry is a check nobody
runs. Caveats are for what cannot be asked of the running system.

The `[[expect]]` array itself is [the on-image self-test](self-test.md). That page
covers which layers take one, the check kinds, and how the checks reach the image.

### A feature selection is a build point, not a new recipe

The feature axis is a list, so the number of *legal* selections grows exponentially in
the number of features. Most of them are nobody's curated point. "The shipped H96
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
solved package manifest. `build` compiles it in its own work directory under a distinct
image identity, so two selections can coexist without one landing on the
other's artifacts. Every lock-reading command takes the reference, so `why-rebuild`,
`verify-patches`, `verify-sources`, and `clean` all work on a variant unchanged. A
variant's first `update` inherits the recipe's pins, so it starts from the same kernel,
u-boot, and blob commits the recipe was pinned at.

Three things follow from a variant being a build point rather than a recipe:

- **It carries no support claim.** The claim belongs to the recipe, and a different
  feature set is a different build. `list-recipes` and the support matrix show only
  authored recipes, and a variant appears in neither.
- **Feature order is significant, so it is preserved.** `config_fragments` and
  `patch_series` compose in selection order, so a later feature wins a kconfig
  conflict. Two orderings of one set are two references — sorting them into one name
  would give two materially different builds a single identity.
- **A selection with no lock is an error, not an implicit `update`.** `build` reads
  locks and never resolves one. The error names the `update` line to run.

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

`core` is pure and testable without a Linux host. All side effects (the filesystem,
subprocesses, the network) live in `engine`.
