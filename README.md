# boot2deb

> A build system for Debian device images — for your laptop, SBC, tablet, or TV box.

boot2deb describes a device as **layered TOML config** and builds a bootable Debian image
from it: compile the kernel and bootloader (or install the distro's), bootstrap the rootfs,
assemble the disk. A build is a *point* across axes — device × kernel × u-boot × suite ×
features × layout — resolved from those layers and pinned into one committed lockfile.

It is a typed, unit-tested Rust workspace, and every axis and layer merge is validated
before a build runs. It ships recipes for six boards, which are both usable images and
**worked examples of unlike hardware**: what makes a Chromebook different from a compute
module is visible in the config rather than buried in a script.

## Status

Under active development, with breaking changes between commits. Builds run mostly on an
x86_64 Pop!_OS laptop and a few on a Turing RK1 running an image boot2deb built.

**What has actually been booted is a per-recipe, per-pin claim**, and it is generated from
the locks rather than written by hand — so read it from the tool, not from prose:

```sh
boot2deb support-matrix
```

Each recipe carries a status — `validated` (an image from these exact pins booted on the
hardware), `expected` (derived from a validated sibling along an axis not expected to change
the outcome), or `experimental` (under active bring-up) — plus the date the claim was last
established. Re-pinning retires a `validated` claim, because the evidence was about the pins
that moved. The published rendering is the
[support matrix](https://gregordinary.github.io/boot2deb/reference/support-matrix.html);
nothing here restates it. No board has been through a full sweep of its peripherals, and an
absent claim is not a claim that the hardware does not work.

## AI disclosure

boot2deb was developed by AI, primarily Claude Code. Human involvement was mostly limited to
setting project goals and validating images on hardware. This is a side project to support a
hobby and comes with no guarantee of quality, accuracy, or update frequency.

## Two properties it is built around

- **Rootless.** Cross-architecture package builds and the Debian bootstrap run in a
  rootless, in-process user-namespace sandbox (plus `qemu-user` when cross-building), and
  the disk image is assembled with no root and no loop devices: GPT tables, `.xz`
  compression, and the ext4 filesystem are all pure Rust. An x86_64 host builds an arm64
  image without `sudo`.
- **Reproducible.** The `.lock` pins every input — source commits, firmware-blob hashes, and
  the solved apt manifest — and each image ships a provenance manifest recording exactly what
  went into it, down to the boot2deb commit that built it. Package churn in a rolling suite
  is pinned against `snapshot.debian.org` on demand. See
  [Reproducibility](https://gregordinary.github.io/boot2deb/reference/reproducibility.html).

## The boards, and why these boards

The six shipped configurations are deliberately unalike: between them they exercise every
axis of the model, so the nearest example to a board you care about is usually one of them.

| Board | SoC / arch | What this example shows |
| --- | --- | --- |
| [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html) | RK3588 / arm64 | The full pipeline: a patched mainline kernel, u-boot written into the disk's raw gap, and an optional Rockchip media userspace (MPP + RGA + ffmpeg-rk) built in a target-arch sandbox |
| [H96 MAX M9](https://gregordinary.github.io/boot2deb/boards/h96-max-m9.html) | RK3576 / arm64 | A board mainline does not know: its device tree ships with the config, its Wi-Fi driver is an out-of-tree `kmods/` layer, and its u-boot is its own axis — including recipes whose only deliverable is a maskrom-streamable bootloader |
| [ASUS Chromebook C201](https://gregordinary.github.io/boot2deb/boards/asus-c201.html) | RK3288 / armhf | The opposite extreme: 32-bit, Debian's own kernel, ChromeOS firmware in SPI. It compiles nothing, its lock pins nothing from git, and the deliverable is a vboot-signed kernel in a ChromeOS partition — with A/B slots, so a bad kernel upgrade costs one reboot |
| [ASUS C100P](https://gregordinary.github.io/boot2deb/boards/asus-c100p.html) · [Chromebit CS10](https://gregordinary.github.io/boot2deb/boards/asus-chromebit-cs10.html) | RK3288 / armhf | What a board costs once its family is here: one device file each, no overlay, no kernel, no code — everything shared lives on the SoC layer |
| [RK3576 EVB1 v10](https://gregordinary.github.io/boot2deb/boards/rk3576-evb1-v10.html) | RK3576 / arm64 | The reference board beside the retail one, sharing a SoC layer and a kernel while carrying none of the TV box's peripherals |

The RK1's headline capability — hardware video transcode — has its kernel side shipped and
its userspace building, but no boot2deb image has been measured transcoding on it.

## Quick start

Build the base Turing RK1 image on an x86_64 or arm64 Debian/Ubuntu host. The build is
rootless — no `sudo`.

1. Install Rust ([rustup.rs](https://rustup.rs)), clone this repo, and install the binary:

   ```sh
   cd boot2deb
   cargo install --path crates/cli    # puts `boot2deb` on your PATH
   ```

   The crate is `boot2deb-cli`; the binary it installs is `boot2deb`. Every command below
   — and every hint the tool itself prints — assumes it is on `PATH`. Working from a
   checkout without installing, prefix them with `cargo run -p boot2deb-cli --`.

2. Ask `doctor` what your host is missing. It probes for every build tool the recipe will
   actually invoke and prints the exact install command for *your* distro:

   ```sh
   boot2deb doctor turing-rk1/forky
   ```

   Run the lines it reports, then re-run until every check passes.

3. Build. This compiles the kernel and u-boot, bootstraps the Debian rootfs, and writes a
   bootable disk image (tens of minutes cold; cached after):

   ```sh
   boot2deb build turing-rk1/forky
   ```

   The final lines print the image path under `build/turing-rk1/forky/artifacts/` and a
   unique first-boot password for user `debian` — note it down, or authorize your SSH key
   in the recipe and skip typing it. For hardware video transcode, build
   `turing-rk1/media-accel-forky` instead.

4. Flash it. This is board-specific — for the RK1 it is the Turing Pi BMC (`tpi` or the web
   UI), or a removable card. See [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html).

Full walkthrough: [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html).

## Making it yours

A shipped recipe is a starting point, not a ceiling. `list-recipes` shows what is authored;
most changes need no new file at all.

**Compose features a-la-carte.** Name any selection on `update`/`build` and it is pinned and
built as its own point, with its own lock, beside the recipe it starts from:

```sh
boot2deb update turing-rk1/forky --feature media-accel-rockchip --feature jellyfin
boot2deb build  turing-rk1/forky+media-accel-rockchip+jellyfin
```

**Keep your own work out-of-tree.** An overlay directory holds your devices, kernels, and
recipes, wins over the shipped tree name-for-name, and takes the locks `update` writes — so
there is nothing to fork and nothing to rebase. A `base.toml` there is also where your own
SSH keys belong, so every image you build authorizes you without editing the shipped tree:
[the account, sudo, and SSH keys](https://gregordinary.github.io/boot2deb/access.html).

The tutorials take these in order:

- [Adapting a shipped recipe](https://gregordinary.github.io/boot2deb/tutorials/adapting-a-recipe.html)
  — a different suite, feature set, or localization, from a build flag up to a device of your own.
- [Moving a board to a newer kernel](https://gregordinary.github.io/boot2deb/tutorials/newer-kernel.html)
  — measure whether a patch series survives a kernel you have not adopted (it changes no
  pin), encode the boundary, then adopt it.
- [Authoring a recipe](https://gregordinary.github.io/boot2deb/tutorials/authoring-a-recipe.html)
  — name a build point, and declare what it has been taken through.
- [Adding a board](https://gregordinary.github.io/boot2deb/contributing/adding-a-board.html)
  — bring up hardware that is not here yet, starting from `boot2deb new-device`.

## How it works

- **Config model** — the hardware stack resolves `arches ← socs ← boot-methods ← devices`,
  with the kernel, the u-boot series, out-of-tree modules (`kmods/`), and rootfs features
  (`features/`) as orthogonal axes.
  [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html).
- **Recipes and locks** — a *recipe* pins a build point by name; `update` is the only command
  that consults upstream and writes a sibling `.lock`; `build` reads only that lock.
- **Kernel patches** — version-coupled patch series and kconfig fragments live on the kernel
  axis and are applied behind a verify-applies gate. A series declares which kernel versions
  it claims, and `verify-patches --kernel` measures a version it does not.
  [Adding a patch](https://gregordinary.github.io/boot2deb/contributing/adding-a-patch.html).
- **Not every board needs every stage** — a build compiles a kernel only if the board needs
  one of its own, and a bootloader only if the board's firmware is ours to make. The model
  states what is true of each board rather than making them look alike.

## Documentation

The full documentation is published as a book at
**[gregordinary.github.io/boot2deb](https://gregordinary.github.io/boot2deb/)**. The sources
live in [`docs/`](docs/); build them locally with `mdbook serve docs`.

- [Introduction](https://gregordinary.github.io/boot2deb/introduction.html)
- User guide — [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html),
  [Upgrading the kernel](https://gregordinary.github.io/boot2deb/kernel-upgrades.html),
  [Locale, timezone, and keyboard](https://gregordinary.github.io/boot2deb/localization.html)
- Tutorials — [Adapting a shipped recipe](https://gregordinary.github.io/boot2deb/tutorials/adapting-a-recipe.html),
  [Moving a board to a newer kernel](https://gregordinary.github.io/boot2deb/tutorials/newer-kernel.html),
  [Authoring a recipe](https://gregordinary.github.io/boot2deb/tutorials/authoring-a-recipe.html)
- Boards — [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html),
  [H96 MAX M9](https://gregordinary.github.io/boot2deb/boards/h96-max-m9.html),
  [RK3576 EVB1 v10](https://gregordinary.github.io/boot2deb/boards/rk3576-evb1-v10.html),
  [ASUS C201](https://gregordinary.github.io/boot2deb/boards/asus-c201.html),
  [ASUS C100P](https://gregordinary.github.io/boot2deb/boards/asus-c100p.html),
  [ASUS Chromebit CS10](https://gregordinary.github.io/boot2deb/boards/asus-chromebit-cs10.html)
- Reference — [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html),
  [CLI](https://gregordinary.github.io/boot2deb/reference/cli.html),
  [Support matrix](https://gregordinary.github.io/boot2deb/reference/support-matrix.html),
  [Overlays](https://gregordinary.github.io/boot2deb/reference/overlays.html),
  [Image identity](https://gregordinary.github.io/boot2deb/reference/image-identity.html),
  [Reproducibility](https://gregordinary.github.io/boot2deb/reference/reproducibility.html)
- Contributing — [Adding a board](https://gregordinary.github.io/boot2deb/contributing/adding-a-board.html),
  [Adding a patch](https://gregordinary.github.io/boot2deb/contributing/adding-a-patch.html)

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

boot2deb is licensed under the GNU General Public License v3.0 or later — see
[`LICENSE`](LICENSE). Vendored third-party components (the Rockchip `rkbin` firmware blobs,
the boot and kernel-hook scripts, and the Debian archive keyring) keep their own licenses;
see [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
