# Adding a board

Bringing up a new device is mostly **writing config layers** — a build resolves from TOML
across the [config model](../reference/config-model.md)'s axes, so a new board is a set of
small TOML files plus any vendored blobs and kernel fragments. The one exception is a
genuinely new *chip family*, which also needs a small Rust change; see
[What needs code](#what-needs-code).

> **Which track are you on?** This is the **bring-up track** (resolve → update → verify →
> build) — for a board or patch that has no lock yet. If you only want to build one of the
> shipped recipes, take the shorter [Getting started](../getting-started.md) track
> (doctor → build) instead. To add a *patch* rather than a board, see
> [Adding a patch](adding-a-patch.md).

> **Start with the generator.** `boot2deb new-device <name>` scaffolds the device
> (and a matching recipe) for you — it offers the valid SoC/boot-method/kernel/feature
> choices, fills every derivable value, and leaves the researched ones marked `# TODO:`.
> Run it (`--soc <soc>` non-interactively, or answer the prompts on a terminal; add
> `--overlay <dir>` to scaffold into your own tree), then edit the TODO values below.
> The rest of this page explains what those files mean and which values you must
> research. See [`new-device`](../reference/cli.md#scaffolding).

## Where your files go: overlay or in-tree

Two ways to add a board, chosen by intent:

- **Out-of-tree overlay** — you are bringing up a board for yourself. Put the files in
  your own directory and pass `--overlay <dir>`; there is nothing to fork, and the board's
  lock is written back beside it. This is the third-party path — see
  [Overlays](../reference/overlays.md).
- **In-tree** — you are contributing a board back to boot2deb. Add the files to the
  vendored tree and open a pull request.

The files are identical either way; only their location differs. The rest of this page
describes those files.

## What needs code

Most of a board is data, but three axes are **closed Rust enums** — `Arch`, `Soc`, and
`BootMethod` in `crates/core/src/model.rs` — chosen for type safety and exhaustiveness
checking. A board built on a chip family that already exists (any RK35xx SoC, the
`rockchip-rkbin` boot method) needs **no code**: the variant is already there. A genuinely
new family does:

- **New SoC** (e.g. a non-Rockchip chip) — add a variant to the `Soc` enum near the top of
  `model.rs` *and* to its `kebab_enum!` invocation grouped just below the enum
  definitions, then rebuild. The compiler flags every `match` that must now handle it.
- **New architecture** or **new boot method** — the same, on `Arch` / `BootMethod`. A new
  boot method also needs the engine taught how to write its payloads.

This is a deliberate boundary: closed enums give the compiler a single source of truth and
catch a half-added target at compile time, at the cost of a recompile for a new family.
Within an existing family it is pure config.

## The layers to write

Work from the bottom of the hardware stack up, adding only what is new:

1. **arch** (`arches/<arch>.toml`) — only for a CPU architecture not already present.
   Arch-wide kbuild facts: the cross triple, the `ARCH=` values for kbuild and u-boot, the
   kernel image path.
2. **soc** (`socs/<soc>.toml`, plus `socs/<soc>/overlay/` for files baked into the rootfs)
   — the SoC's shared properties: device-tree directory, force-loaded modules, arch, and
   any SoC-wide firmware packages.
   - **Media-accel sources are optional and ride the feature.** Supply the
     `[userspace.mpp]`, `[userspace.librga]`, `[userspace.libmali]`, `[ffmpeg.base]`, and
     `[ffmpeg.rockchip]` stanzas here **only if** a board of this SoC will enable a
     `media-accel-*` feature (the feature compiles them into `.deb`s); copy the block from
     `socs/rk3588.toml`. A headless SoC that never transcodes omits them entirely. Selecting
     a `requires_media_accel` feature on a SoC that lacks them is a resolve-time error, so
     the coupling is checked, not assumed.
3. **boot-method** (`boot-methods/<method>.toml`) — how this family boots: the u-boot
   source + ref, and the **image offsets** (where `idbloader` and `u-boot.itb` sit in the
   raw gap, and where the rootfs partition starts). `boot-methods/<method>/overlay/` ships
   any boot-time files (e.g. the extlinux generator).
4. **device** (`devices/<device>.toml`) — the board itself, stating only its deltas: its
   `soc`, `boot_method`, `supported_boot_methods`, `uboot_defconfig`, `kernel_dtb`,
   `image_size`, `hostname`, `supported_kernels` / `default_kernel`, `default_suite`,
   `default_layout`, and any board-memory-specific bootloader blobs (the `[rkbin]` DDR TPL
   + ATF for a Rockchip board — these are board-specific, so they live here, not at the soc
   layer).
   - **`device_config_fragments` gotcha:** naming a fragment here makes its file
     *mandatory*. `device_config_fragments = ["device/my-board"]` requires
     `fragments/device/my-board.config` to exist — a missing file fails `resolve`. A board
     with no board-specific kconfig deltas uses `device_config_fragments = []` to add none.
     Do not name a fragment you have not written.
5. **kernel** (`kernels/<kernel>.toml`) — the orthogonal kernel axis: its source refs,
   `.config` fragments, and patch profile. Version-coupled, so a new kernel version is a
   new file.

Supporting assets:

- **blobs** (`blobs/<soc>/`) — vendored bootloader binaries the device/boot-method
  references.
- **fragments** (`fragments/<name>.config`) — kernel `.config` fragments merged onto the
  base defconfig, referenced by name from a kernel or device.
- **patch profile** — lives in the separate `patches` repo, referenced by the kernel; see
  [Adding a patch](adding-a-patch.md).

Finally, a **recipe** (`recipes/<recipe>.toml`) pins one point across the axes — device,
kernel, suite, features, layout.

## Values you must research

Most fields fail loudly at `resolve`: bad image geometry, a missing fragment, or an
unvendored keyring are all caught up front. **Two fields are not validated** until the
stage that consumes them compiles, so a typo produces a late, confusing failure:

| Value | Layer | Fails at |
| --- | --- | --- |
| `kernel_dtb` | device | the kernel build — the DTB is not produced |
| `uboot_defconfig` | device | the u-boot build — unknown defconfig |

Take both from the board's upstream support: `kernel_dtb` is the device tree the mainline
kernel builds for the board (under `arch/<arch>/boot/dts/<dt_dir>/`), and
`uboot_defconfig` is the board's u-boot defconfig. Confirm each exists in the exact
kernel/u-boot versions you pin before you trust a green `resolve`.

## Bring it up

With the layers written, use the CLI's checks as guardrails, in order:

```sh
# 1. Does it resolve to a coherent build point? Also runs the geometry / fragment /
#    keyring preflight, so this is a real coherence gate, not just a merge print.
cargo run -p boot2deb-cli -- resolve <recipe>

# 2. Resolve upstream refs + hash blobs into the lock.
cargo run -p boot2deb-cli -- update <recipe> --kernel-ref <ref>

# 3. Is the host equipped to build it?
cargo run -p boot2deb-cli -- doctor <recipe>

# 4. Does the patch series apply cleanly to the pinned kernel? Auto-fetches the locked
#    kernel — no hand-cloned tree — or add --kernel-src ../linux if you have a checkout.
cargo run -p boot2deb-cli -- verify-patches <recipe>

# 5. Does the .config generate (and, with --reference-config, match a reference)?
cargo run -p boot2deb-cli -- verify-config <recipe>

# 6. Build.
cargo run -p boot2deb-cli -- build <recipe>
```

`resolve`, `update`, and the two `verify-*` commands fail with a typed error before any
compile starts, so most config mistakes surface in seconds rather than partway through a
build. The `verify-*` commands auto-fetch the pinned source trees, so this whole sequence
works on a fresh clone with no hand-cloned kernel — see
[Verification](../reference/cli.md#verification).

## A worked example: a second RK3588 board

A board on an existing SoC needs only a **device** file and a **recipe** — arch, soc, and
boot-method all reuse the shipped layers. Suppose `my-board` is another RK3588 module.

`devices/my-board.toml`:

```toml
description             = "My RK3588 board"
soc                     = "rk3588"                        # reuse the shipped SoC layer
boot_method             = "rockchip-rkbin"
supported_boot_methods  = ["rockchip-rkbin"]
uboot_defconfig         = "my-board-rk3588_defconfig"    # research: must exist in u-boot
kernel_dtb              = "rockchip/rk3588-my-board.dtb" # research: must exist in the kernel
device_config_fragments = []                             # no board-specific kconfig deltas
supported_kernels       = ["rk3588-mainline-7.1"]
default_kernel          = "rk3588-mainline-7.1"
default_suite           = "forky"
default_layout          = "combined"
hostname                = "my-board"
image_size              = "2G"

[rkbin]                                                  # board-memory-specific DDR init
atf = "rk3588_bl31_v1.51.elf"
tpl = "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin"
```

`recipes/my-board-forky.toml`:

```toml
device   = "my-board"
kernel   = "rk3588-mainline-7.1"
suite    = "forky"
features = ["media-accel-rockchip"]   # or [] for a plain image
layout   = "combined"
```

Then run the [bring-it-up](#bring-it-up) sequence against `my-board-forky`. A **new SoC**
would additionally need `socs/<soc>.toml` (with the required userspace/ffmpeg stanzas) and
its fragments; a **new family** would need the code change from
[What needs code](#what-needs-code).

## Document your board

Give each board a page under [Boards](../boards/turing-rk1.md), the way the Turing RK1
page does, since flashing is inherently per-board — a Turing Pi module flashes through the
BMC, a standalone SBC takes an SD card or a maskrom loader, a laptop boots UEFI. A useful
skeleton:

```markdown
# <Board name>

The `<recipe>` recipe builds a bootable Debian <suite> image for the <board> (<SoC>).
It pins kernel `<ver>`, u-boot `<ver>`, and <features>.

Build it as in [Getting started](../getting-started.md):

    cargo run -p boot2deb-cli -- build <recipe>

## Flash
<how this board takes an image: card reader, BMC, maskrom, UEFI…>

## Serial console
<UART pins / adapter / baud>

## First boot
<credentials, resize-on-first-boot, hostname>
```
