# Config model

A build is a single point across the axes a user selects:

**device × kernel × suite × layout, plus composable features**

- **device** — the target hardware. It resolves through a layered hardware stack (see
  below).
- **kernel** — an orthogonal axis that owns everything version-coupled: its source
  refs, `.config` fragments, and [patch profile](#patch-profiles-belong-to-the-kernel).
  A device declares which kernels it supports and a default; override with `--kernel`
  (values from `list-kernels`).
- **suite** — the Debian suite (e.g. `forky`, `sid`); override with `--suite`.
- **layout** — how the disk image is packaged: `combined` (one whole-disk image with
  the bootloader in the raw gap) or `split` (separate bootloader and rootfs images for
  a two-medium install); override with `--layout`.
- **features** — a *list* of composable rootfs add-ins stacked onto the base image:
  a **capability** feature that provides a hardware stack (`media-accel-rockchip`, the
  RK35xx HW-transcode userspace) or an **application** feature that installs an app
  (`jellyfin`). Features are the knob the two shipped recipes differ by —
  `turing-rk1-forky` and `turing-rk1-jellyfin` share a device and kernel and differ
  only here. Override with `--feature` (repeatable; values from `list-features`).

Two more knobs round out a build without being headline axes: `--boot-method` (a
device property, rarely overridden) and `--image-size`.

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
exact resolved pins: commit hashes for every source (including the `[patches]` profile
and commit), blob content hashes, and the solved rootfs manifest digest.

The split between the two is what makes a build reproducible:

- **`update`** is the only command that consults upstream. It resolves refs to commits,
  hashes blobs, and writes the lock.
- **`build`** reads only the lock. It touches no network for its pins, so the same lock
  always produces the same inputs.

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
