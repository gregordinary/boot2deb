# boot2deb

> Build Debian images for your laptop, SBC, tablet, or other device.

## Status

This project is under active development. There may be breaking changes between commits. The image build process has been primarily tested on an AMD x86_64 laptop running Pop!_OS. A few builds have been performed on a Turing RK1 running a Debian image built by boot2deb. The Turing RK1 is the most tested target (hardware video transcode validated on real hardware); the ASUS Chromebook C201 boots to a login shell with Wi-Fi on both Debian suites it targets. I haven't yet tested the full hardware functionality of the C201, but in theory it is fully supported.

## AI disclosure

boot2deb was developed by AI, primarily Claude Code (Opus 4.8). Human involvement was
mostly limited to setting project goals and validating images on hardware. This is a
side project to support a hobby and comes with no guarantee of quality, accuracy, or
update frequency.

## What it is

boot2deb resolves a *build point* from layered TOML config (arch ← soc ← boot-method ←
device, plus an orthogonal kernel axis) and drives the whole pipeline from one committed
lockfile: compile the kernel and bootloader (or install the distro's), bootstrap the
Debian rootfs, and assemble a bootable disk image. It is a typed, unit-tested Rust
workspace; every axis and layer merge is validated before a build runs.

Two properties it is built around:

- **Rootless.** Cross-architecture package builds and the Debian bootstrap run in a
  rootless sandbox (`mmdebstrap --mode=unshare` + `bwrap` + `qemu-user`), and the disk
  image is assembled with no root and no loop devices: GPT tables and `.xz` compression
  are pure Rust, and the ext4 filesystem is formatted with `mke2fs -d` inside an
  unprivileged user namespace. An x86_64 host builds an arm64 image without `sudo`.
- **Reproducible.** The `.lock` pins every input — source commits, firmware-blob hashes,
  and the solved apt manifest — and each image ships a provenance manifest recording
  exactly what went into it, down to the boot2deb commit that built it. Package churn in
  a rolling suite is pinned against `snapshot.debian.org` on demand. See
  [Reproducibility](https://gregordinary.github.io/boot2deb/reference/reproducibility.html).

## Supported boards

| Board | SoC | Arch | Status |
| --- | --- | --- | --- |
| [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html) | RK3588 | arm64 | Boots; hardware video transcode validated |
| [ASUS Chromebook C201](https://gregordinary.github.io/boot2deb/boards/asus-c201.html) | RK3288 | armhf | Boots to login + Wi-Fi (forky & trixie) |
| [ASUS Chromebook Flip C100P](https://gregordinary.github.io/boot2deb/boards/asus-c100p.html) | RK3288 | armhf | Image builds; hardware boot not yet confirmed |
| [ASUS Chromebit CS10](https://gregordinary.github.io/boot2deb/boards/asus-chromebit-cs10.html) | RK3288 | armhf | Image builds; hardware boot not yet confirmed |

Each board ships one or more *recipes*, a device plus a Debian suite and any optional
features. The RK1, for example, comes as a base image (`turing-rk1-forky`), a
hardware-transcode image that adds the Rockchip MPP/RGA/ffmpeg userspace
(`turing-rk1-media-accel-forky`), and a Jellyfin image — each with a `trixie` sibling.
List them with `cargo run -p boot2deb-cli -- list-recipes`.

## Quick start

Build the base Turing RK1 image on an x86_64 or arm64 Debian/Ubuntu host. The build is
rootless — no `sudo`.

1. Install Rust ([rustup.rs](https://rustup.rs)) and clone this repo.

2. Ask `doctor` what your host is missing. It probes for every build tool and prints the
   exact install command for *your* distro:

   ```sh
   cd boot2deb
   cargo run -p boot2deb-cli -- doctor turing-rk1-forky
   ```

   Run the lines it reports, then re-run until every check passes.

3. Build. This compiles the kernel and u-boot, bootstraps the Debian rootfs, and writes a
   bootable disk image (tens of minutes cold; cached after):

   ```sh
   cargo run -p boot2deb-cli -- build turing-rk1-forky
   ```

   The final lines print the image path under `build/turing-rk1-forky/artifacts/` and a
   unique first-boot password for user `debian` — note it down. For hardware video
   transcode, build `turing-rk1-media-accel-forky` instead.

4. Flash it. This is board-specific — for the RK1 it is the Turing Pi BMC (`tpi` or the
   web UI), or a removable card. See [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html).

Full walkthrough: [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html).

## How it works

- **Config model** — a build is a point across device × kernel × suite × features ×
  layout, resolved by merging TOML layers (`arches/ socs/ boot-methods/ devices/`, with
  the kernel as an orthogonal axis). [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html).
- **Recipes and locks** — a *recipe* pins a build point by name; `update` writes a
  sibling `.lock` with the exact resolved pins, and `build` reads only that lock.
- **Kernel patches** — version-coupled patch series and kconfig fragments live on the
  kernel axis and are applied behind a verify-applies gate; `verify-sources` flags any
  pin that is not durably re-fetchable.
  [Adding a patch](https://gregordinary.github.io/boot2deb/contributing/adding-a-patch.html).
- **Your own boards** — keep out-of-tree devices and recipes in an overlay directory
  instead of forking. [Overlays](https://gregordinary.github.io/boot2deb/reference/overlays.html).

## Documentation

The full documentation is published as a book at
**[gregordinary.github.io/boot2deb](https://gregordinary.github.io/boot2deb/)**. The
sources live in [`docs/`](docs/); build them locally with `mdbook serve docs`. Chapters:

- [Introduction](https://gregordinary.github.io/boot2deb/introduction.html)
- [Getting started](https://gregordinary.github.io/boot2deb/getting-started.html) — prerequisites and your first build
- [Upgrading the kernel](https://gregordinary.github.io/boot2deb/kernel-upgrades.html)
- [Locale, timezone, and keyboard](https://gregordinary.github.io/boot2deb/localization.html)
- Boards — [Turing RK1](https://gregordinary.github.io/boot2deb/boards/turing-rk1.html),
  [ASUS C201](https://gregordinary.github.io/boot2deb/boards/asus-c201.html),
  [ASUS C100P](https://gregordinary.github.io/boot2deb/boards/asus-c100p.html),
  [ASUS Chromebit CS10](https://gregordinary.github.io/boot2deb/boards/asus-chromebit-cs10.html)
- Reference — [Config model](https://gregordinary.github.io/boot2deb/reference/config-model.html),
  [CLI](https://gregordinary.github.io/boot2deb/reference/cli.html),
  [Overlays](https://gregordinary.github.io/boot2deb/reference/overlays.html),
  [Image identity](https://gregordinary.github.io/boot2deb/reference/image-identity.html),
  [Reproducibility](https://gregordinary.github.io/boot2deb/reference/reproducibility.html)
- Contributing — [Adding a board](https://gregordinary.github.io/boot2deb/contributing/adding-a-board.html),
  [Adding a patch](https://gregordinary.github.io/boot2deb/contributing/adding-a-patch.html)

## Repository layout

```
crates/core     typed model, layer resolution + validation, patch-profile / lock /
                kconfig formats (pure, unit-tested)
crates/engine   Linux side effects: git shell-outs, lock resolver, patch verify gate,
                kernel-config generation, the compile stages, the rootfs + image nodes,
                and the host preflight behind `doctor`
crates/cli      the boot2deb binary

arches/ socs/ boot-methods/ devices/ kernels/ recipes/   config layers (TOML)
blobs/ fragments/                                         vendored blobs, kconfig
docs/                                                     the mdBook
```

## License

boot2deb is licensed under the GNU General Public License v3.0 or later — see
[`LICENSE`](LICENSE). Vendored third-party components (the Rockchip `rkbin` firmware
blobs, the boot and kernel-hook scripts, and the Debian archive keyring) keep their own
licenses; see [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
