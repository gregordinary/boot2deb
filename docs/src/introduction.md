# Introduction

boot2deb turns a laptop, SBC, tablet, or other device into a Debian device. It is a
Rust-native, typed, testable builder that resolves a build from layered TOML config
— an `arch ← soc ← boot-method ← device` hardware stack plus an orthogonal kernel
axis — and drives the whole pipeline: kernel, u-boot, media-accel userspace, ffmpeg,
the Debian rootfs, and a bootable disk image, all from a single committed lockfile.

The image assembly is pure Rust: GPT partitioning, ext4 formatting, and `.xz`
compression with no C dependencies and no `sudo`. Cross-architecture package builds
run in a rootless sandbox (`mmdebstrap --mode=unshare` + `bwrap` + `qemu-user`), so
an x86_64 host builds an arm64 image without root.

## Where to start

- **[Getting started](getting-started.md)** — install the prerequisites and build
  your first image.
- **[Turing RK1](boards/turing-rk1.md)** — the shipped RK3588 configuration, and how
  to flash it.
- **[Config model](reference/config-model.md)** — how a build is described across its
  axes, and how the layers resolve.
- **[CLI](reference/cli.md)** — the command reference.
- **[Overlays](reference/overlays.md)** — keep your own boards and retunings out-of-tree.
- **[Adding a board](contributing/adding-a-board.md)** — bring up a new device.
- **[Adding a patch](contributing/adding-a-patch.md)** — get a patch into a build.
