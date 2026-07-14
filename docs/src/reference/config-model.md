# Config model

A build is a single point across the axes a user selects:

**device × kernel × suite × layout, plus composable features**

- **device** — the target hardware. It resolves through a layered hardware stack (see
  below).
- **kernel** — an orthogonal axis that owns everything version-coupled: its source
  refs, `.config` fragments, and [patch profile](#patch-profiles-belong-to-the-kernel).
  A device declares which kernels it supports and a default; override with `--kernel`
  (values from `list-kernels`). Some kernels are [not built at
  all](#kernels-are-compiled-or-installed).
- **suite** — the Debian suite (e.g. `forky`, `sid`); override with `--suite`.
- **layout** — how the disk image is packaged: `combined` (one whole-disk image, boot
  payload and rootfs on a single medium) or `split` (separate bootloader and rootfs
  images for a two-medium install); override with `--layout`. Only a boot method that
  *has* a bootloader can split it off; the combination is rejected at resolve for one
  that does not.
- **features** — a *list* of composable rootfs add-ins stacked onto the base image:
  a **capability** feature that provides a hardware stack (`media-accel-rockchip`, the
  RK35xx HW-transcode userspace) or an **application** feature that installs an app
  (`jellyfin`). Features are the knob the two shipped recipes differ by —
  `turing-rk1-forky` and `turing-rk1-jellyfin` share a device and kernel and differ
  only here. Override with `--feature` (repeatable; values from `list-features`).

Three more knobs round out a build without being headline axes: `--boot-method` (a
device property, rarely overridden), `--board` (the depthcharge board profile — see
[Boot methods describe different things](#boot-methods-describe-different-things)), and
`--image-size`.

The system locale, timezone, and console keymap are resolved the same way, and are split
across two layers for a reason: see
[Locale, timezone, and keyboard](../localization.md).

## Kernels are compiled, or installed

A kernel definition's `flavor` decides what shape it has, because the two kinds of
kernel have almost nothing in common:

- **`mainline` / `vendor`** — compiled from source. The definition owns a source ref, a
  base defconfig, a fragment list, and a patch profile, and the build clones the tree,
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
`asus-c201-forky` and `asus-c201-trixie` name the same `debian-armmp` kernel and resolve
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
  GPT for a type GUID. The layer carries that partition's geometry and the GPT attribute
  bits that make the firmware boot it (`priority` / `tries` / `successful`), plus the
  command line to bake into the signature. The device carries a **board profile**.

Because the requirements are method-scoped, a board is only ever asked for fields its
own boot method reads: the C201 declares no `uboot_defconfig` and no rkbin blobs, and
omitting them is not an error — omitting its `[depthcharge]` block is.

### Board profiles

A depthcharge **board profile** is `depthcharge-tools`' codename for a *firmware
behaviour set* — its payload ceiling, and whether it loads a FIT ramdisk or needs the
initramfs address patched into every DTB. It describes **the firmware a unit runs**, not
the board model: the same C201 takes one profile on stock firmware and another with
libreboot installed. So the device declares a default and the profiles it supports, and
`--board` selects among them.

The default is deliberately the *stock* profile: a stock-profile image boots on stock
firmware **and** on a unit running libreboot, while the reverse is not true.

## Patch profiles belong to the kernel

A **patch profile** (e.g. `rk3588-accel`) is the ordered patch series applied to the
source trees before they compile. It is **a property of the kernel definition, not a
user-selected axis**: a kernel names its profile via `patch_profile` in
`kernels/<id>.toml`, and there is deliberately no `--profile` flag, because a series
that applies to one kernel version does not apply to another — so the profile is
version-coupled to the kernel that owns it. Profiles live in a separate `patches` repo,
not in this one; the resolved profile name and its exact `patches`-repo commit are
recorded together in the lock's `[patches]` block. Authoring workflow:
[Adding a patch](../contributing/adding-a-patch.md).

A kernel may apply **no series at all** — a stock mainline kernel whose SoC is fully
upstream, or a vendor tree that already ships its patches. It writes
`patch_profile = "none"`, and then the build never reads the `patches` repo: nothing is
fetched, nothing is applied, `verify-patches` reports there is nothing to verify, and
the lock **omits its `[patches]` block entirely** rather than pinning a commit the build
never consumes. Such a board builds on a machine with no `patches` checkout.

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
arches/  socs/  boot-methods/  devices/  kernels/  recipes/
```

with vendored bootloader blobs under `blobs/<soc>/`, kernel `.config` fragments under
`fragments/`, and the resolved exact pins in `recipes/<recipe>.lock`.

### Media-accel sources ride the feature, not the SoC

The `[userspace]` (MPP/RGA/Mali) and `[ffmpeg]` source stanzas at the soc layer are
**optional**. They provide the trees the `media-accel-rockchip` feature compiles, and
they are copied into a build only when a selected feature declares
`requires_media_accel`. A recipe that builds no transcode stack carries no such sources
and skips the userspace/ffmpeg compile nodes entirely; a SoC that never transcodes omits
the stanzas. Selecting a `requires_media_accel` feature on a SoC that lacks them is a
resolve-time error, so the coupling is checked, not assumed.

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

A **recipe** (`recipes/<recipe>.toml`) pins one buildable point: it names the device
and, optionally, the kernel, suite, features, layout, and image size (each omitted axis
falls back to the device default). Its **lock** (`recipes/<recipe>.lock`) holds the
exact resolved pins: for every git source, the repo URL it was pinned from plus the
ref and commit, blob content hashes, and the solved rootfs manifest digest.

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
manifest = "asus-c201-forky.pkgs.lock"
```

That is the whole truth about what it depends on, and the package manifest beside it
pins every one of those packages by name, version, and sha256.

The split between the two is what makes a build reproducible:

- **`update`** is the only command that consults upstream. It resolves refs to commits,
  hashes blobs, and writes the lock.
- **`build`** reads only the lock. It touches no network for its pins, so the same lock
  always produces the same inputs. Before building it checks the lock against a fresh
  resolution on every axis the lock records from config — the source repos, blob file
  names, kernel id, suite, patch profile, extra debs — and refuses on drift, so a
  config edit after `update` (say a boot-method flip to a different u-boot repo) is a
  named error rather than a build against stale pins.

See the [CLI reference](cli.md) for the commands that operate on these.

## Crates

The builder is a Rust workspace of three crates:

```
crates/core     typed model, layer resolution + validation, patch-profile / lock /
                kconfig formats (pure, deterministic, unit-tested — no Linux host)
crates/engine   Linux side effects: git shell-outs, the lock resolver, the patch
                verify gate, kernel-config generation, the compile stages (kernel /
                u-boot / userspace / ffmpeg), the rootfs + image nodes, and the host
                preflight behind `doctor`
crates/cli      the boot2deb binary
```

`core` is pure and testable without a Linux host; all side effects (the filesystem,
subprocesses, the network) live in `engine`.
