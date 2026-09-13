# boot2deb

> A build system for Debian device images, for your laptop, SBC, tablet, or TV box.

boot2deb describes a device as **layered TOML config** and builds a bootable Debian image
from it. It compiles the kernel and bootloader or installs the distribution's, bootstraps
the rootfs, and assembles the disk. A build is a *point* across axes: device × kernel ×
u-boot × suite × features × layout. Those layers resolve to that point, and one committed
lock pins it.

It is a typed, unit-tested Rust workspace, and every axis and layer merge is validated
before a build runs. It ships recipes for six boards, which are usable images and **worked
examples of unlike hardware**. What makes a Chromebook different from a compute module is
visible in the config.

## Status

Under active development, with breaking changes between commits. Builds run mostly on an
x86_64 Pop!_OS laptop and a few on a Turing RK1 running an image boot2deb built.

**What has actually been booted is a per-recipe, per-pin claim.** The tool generates it from
the locks rather than from prose, so read it from the tool:

```sh
boot2deb support-matrix
```

Each recipe carries a status and the date its claim was last established:

- `validated`: an image from these exact pins booted on the hardware.
- `expected`: derived from a validated sibling, along an axis not expected to change the
  outcome.
- `experimental`: under active bring-up.

Re-pinning retires a `validated` claim, because the evidence was about the pins that moved.
The published rendering is the
[support matrix](https://gregordinary.github.io/boot2deb/reference/support-matrix.html), and
nothing here restates it. No board has been through a full sweep of its peripherals, and an
absent claim is not a claim that the hardware does not work.

## AI disclosure

boot2deb was developed by AI, primarily Claude Code. Human involvement was mostly limited to
setting project goals and validating images on hardware. This is a side project to support a
hobby, and it comes with no guarantee of quality, accuracy, or update frequency.

## Rootless builds

Cross-architecture package builds and the Debian bootstrap run in a rootless, in-process
user-namespace sandbox. A cross-build adds `qemu-user`. The disk image is assembled with no root and no
loop devices. GPT tables, `.xz` compression, and the ext4 filesystem are
all pure Rust. An x86_64 host builds an arm64 image without `sudo`.

## Reproducible builds

The `.lock` pins every input: source commits, firmware-blob hashes, and the solved apt
manifest. Each image ships a provenance manifest recording exactly what went into it, down
to the boot2deb commit that built it. Package churn in a rolling suite is pinned against
`snapshot.debian.org` on demand. See
[Reproducibility](https://gregordinary.github.io/boot2deb/reference/reproducibility.html).

## Shipped boards

The six shipped configurations are deliberately unalike. Between them they exercise every
axis of the model, so the nearest example to a board you care about is usually one of them.

| Board | SoC / arch | What this example shows |
| --- | --- | --- |
| [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html) | RK3588 / arm64 | The full pipeline: a patched mainline kernel, u-boot written into the disk's raw gap, and an optional Rockchip media userspace (MPP + RGA + ffmpeg-rk) built in a target-arch sandbox |
| [H96 MAX M9](https://gregordinary.github.io/boot2deb/boards/h96-max-m9.html) | RK3576 / arm64 | A board mainline does not know: its device tree ships with the config, its Wi-Fi driver is an out-of-tree `kmods/` layer, and its u-boot is its own axis. Some recipes deliver only a maskrom-streamable bootloader |
| [ASUS Chromebook C201](https://gregordinary.github.io/boot2deb/boards/asus-c201.html) | RK3288 / armhf | The opposite extreme: 32-bit, Debian's own kernel, ChromeOS firmware in SPI. It compiles nothing, its lock pins nothing from git, and the deliverable is a vboot-signed kernel in a ChromeOS partition. A/B slots make a bad kernel upgrade cost one reboot |
| [ASUS C100P](https://gregordinary.github.io/boot2deb/boards/asus-c100p.html) · [Chromebit CS10](https://gregordinary.github.io/boot2deb/boards/asus-chromebit-cs10.html) | RK3288 / armhf | What a board costs once its family is here: one device file each, no overlay, no kernel, no code. Everything shared lives on the SoC layer |
| [RK3576 EVB1 v10](https://gregordinary.github.io/boot2deb/boards/rk3576-evb1-v10.html) | RK3576 / arm64 | The reference board beside the retail one, sharing a SoC layer and a kernel while carrying none of the TV box's peripherals |

Hardware video transcode is the RK1's headline capability, measured on a boot2deb-built
image. The `h264_rkmpp` and `hevc_rkmpp` encoders produce correct streams. Hardware decode
through `-hwaccel v4l2request` cuts decode CPU cost by 53x at 1080p and up to 143x at 4K.

Jellyfin drives the encoder on the shipped image. Its hardware-decoding list is empty,
because the stock server cannot ask for `v4l2request`. A patched server was measured
driving both halves on the board on 2026-09-02, and the image builds the stock one. See
[Accelerated Jellyfin](https://gregordinary.github.io/boot2deb/jellyfin.html).

On the RK3576 the picture is narrower. Decode and 2D are driven, encode has no mainline
driver at all, and hardware H.264 decode is not yet reliable. See
[H96 MAX M9](https://gregordinary.github.io/boot2deb/boards/h96-max-m9.html).

## Quick start

Build the base Turing RK1 image on an x86_64 or arm64 Debian or Ubuntu host. The build
itself is rootless and needs no `sudo`, though installing host packages does. From a clean
host:

1. Install Rust from [rustup.rs](https://rustup.rs), clone this repository, and install the
   binary. Cargo links it with a host C compiler, so install one first:

   ```sh
   sudo apt install build-essential   # or your distribution's C toolchain
   cd boot2deb
   cargo install --path crates/cli    # puts `boot2deb` on your PATH
   ```

   The crate is `boot2deb-cli`, and the binary it installs is `boot2deb`. Every command
   below assumes it is on `PATH`, and so does every hint the tool prints. To work from a
   checkout without installing, prefix each one with `cargo run -p boot2deb-cli --`.
   That compiler is the only one boot2deb wants from your host. `doctor` cannot report it
   missing, because `doctor` is the binary that did not build.

2. Ask `doctor` what your host is missing:

   ```sh
   boot2deb doctor turing-rk1/forky
   ```

   It probes for every build tool the recipe invokes, and it prints the exact install
   command for *your* distribution. Run the lines it reports, then re-run until every check
   passes.

3. Build. This compiles the kernel and u-boot, bootstraps the Debian rootfs, and writes a
   bootable disk image:

   ```sh
   boot2deb build turing-rk1/forky
   ```

   A cold build takes tens of minutes, and a later one is cached. The final lines print the
   image path under `build/turing-rk1/forky/artifacts/` and a unique first-boot password for
   user `debian`. Note that password down, or authorize your SSH key in the recipe and skip
   typing it. For hardware video transcode, build `turing-rk1/media-accel-forky` instead.

4. Flash it. Flashing is board-specific. For the RK1 it is the Turing Pi BMC (`tpi` or the
   web UI), or a removable card. See
   [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html).

Full walkthrough: [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html).

## Features and overlays

A shipped recipe is a starting point. `list-recipes` shows what is authored, and most
changes need no new file at all.

### Composing features

Name any selection on `update` or `build`. Each selection is pinned and built as its own
point, with its own lock, beside the recipe it starts from:

```sh
boot2deb update turing-rk1/forky --feature media-accel-rockchip --feature jellyfin
boot2deb build  turing-rk1/forky+media-accel-rockchip+jellyfin
```

### Keeping your own work out-of-tree

An overlay directory holds your devices, kernels, and recipes. It wins over the shipped tree
name-for-name and takes the locks `update` writes, so there is nothing to fork and nothing
to rebase. A `base.toml` there is also where your own SSH keys belong, so every image you
build authorizes you without editing the shipped tree. See
[the account, sudo, and SSH keys](https://gregordinary.github.io/boot2deb/access.html).

### Tutorials

The tutorials take these in order:

- [Adapting a shipped recipe](https://gregordinary.github.io/boot2deb/tutorials/adapting-a-recipe.html):
  a different suite, feature set, or localization, from a build flag up to a device of your
  own.
- [Moving a board to a newer kernel](https://gregordinary.github.io/boot2deb/tutorials/newer-kernel.html):
  measure whether a patch series survives a kernel you have not adopted, which changes no
  pin. Then encode the boundary and adopt it.
- [Authoring a recipe](https://gregordinary.github.io/boot2deb/tutorials/authoring-a-recipe.html):
  name a build point, and declare what it has been taken through.
- [Adding a board](https://gregordinary.github.io/boot2deb/contributing/adding-a-board.html):
  bring up hardware that is not here yet, starting from `boot2deb new-device`.

## Configuration model

### Layer resolution

The hardware stack resolves `arches ← socs ← boot-methods ← devices`. The kernel, the u-boot
series, out-of-tree modules (`kmods/`), and rootfs features (`features/`) are orthogonal
axes. See [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html).

### Recipes and locks

A *recipe* pins a build point by name. `update` is the only command that consults upstream
and writes a sibling `.lock`, and `build` reads only that lock.

### Kernel patches

Version-coupled patch series and kconfig fragments live on the kernel axis, and a
verify-applies gate holds them. A series declares which kernel versions it claims, and
`verify-patches --kernel` measures a version it does not. See
[Adding a patch](https://gregordinary.github.io/boot2deb/contributing/adding-a-patch.html).

### Stages a board needs

When the board needs a kernel of its own, the build compiles one. When the board's firmware
is ours to make, the build produces a bootloader. The model states what is true of each
board.

## Documentation

The full documentation is published as a book at
**[gregordinary.github.io/boot2deb](https://gregordinary.github.io/boot2deb/)**. The sources
live in [`docs/`](docs/). To build them locally, run `mdbook serve docs`.

The book carries its own contents page. These are the entry points into it:

| Section | Start here |
| --- | --- |
| Introduction | [What boot2deb is](https://gregordinary.github.io/boot2deb/introduction.html) |
| User guide | [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html) |
| Tutorials | [Adapting a shipped recipe](https://gregordinary.github.io/boot2deb/tutorials/adapting-a-recipe.html) |
| Boards | [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html) |
| Reference | [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html) |
| Contributing | [Adding a board](https://gregordinary.github.io/boot2deb/contributing/adding-a-board.html) |

## Repository layout

```
crates/core     typed model, layer resolution + validation, patch-series / lock /
                kconfig formats (pure, unit-tested)
crates/engine   Linux side effects: git shell-outs, lock resolver, patch verify gate,
                kernel-config generation, the compile stages, the rootfs + image nodes,
                and the host preflight behind `doctor`
crates/cli      the boot2deb binary

arches/ socs/ boot-methods/ devices/ kernels/ kmods/ features/ recipes/
                                                config layers (TOML)
blobs/ fragments/                               vendored blobs, kconfig
docs/                                           the mdBook
```

## License

boot2deb is licensed under the GNU General Public License v3.0 or later. See
[`LICENSE`](LICENSE). Vendored third-party components keep their own licenses, listed in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md). Those components are the Rockchip
`rkbin` firmware blobs, the boot and kernel-hook scripts, and the Debian archive keyring.
