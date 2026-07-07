# boot2deb
## Status
Work in progress. RK1 boots the built Debian image successfully. Image missing mali_csffw.bin.

## AI Disclosure

boot2deb was developed by AI, primarily Claude Code (Opus 4.8). Human involvement was mostly limited to setting project goals and validating images on hardware. This is a side project to support a hobby and comes with no guarantee of quality, accuracy, or update frequency.

## About boot2deb

Turn a laptop, SBC, tablet, or other device into a Debian device. A Rust-native,
typed, testable builder that resolves a build from layered TOML config (arch ← soc ←
boot-method ← device, plus an orthogonal kernel axis) and drives the whole pipeline —
kernel, u-boot, media-accel userspace, ffmpeg, the Debian rootfs, and a bootable disk
image — from a single committed lockfile.

The image assembly is pure Rust: GPT partitioning, ext4 formatting, and `.xz`
compression with no C dependencies and no `sudo`. Cross-architecture package builds
run in a rootless sandbox (`mmdebstrap --mode=unshare` + `bwrap` + `qemu-user`), so an
x86_64 host builds an arm64 image without root.

## Quick start

Build the shipped Turing RK1 image on an x86_64 or arm64 Debian/Ubuntu host. The build
itself is rootless — no `sudo`.

1. Install Rust ([rustup.rs](https://rustup.rs)) and clone this repo.

2. Ask `doctor` what your host is missing. It probes for every build tool and prints
   the exact install command for *your* distro — including the fix for the Ubuntu
   24.04 user-namespace restriction:

   ```sh
   cd boot2deb
   cargo run -p boot2deb-cli -- doctor turing-rk1-forky
   ```

   Run the lines it reports, then re-run until every check passes.

3. Build. This compiles the kernel and u-boot, bootstraps the Debian rootfs, and writes
   a bootable disk image (tens of minutes cold; cached after):

   ```sh
   cargo run -p boot2deb-cli -- build turing-rk1-forky
   ```

   The final lines print the image path under `build/turing-rk1-forky/artifacts/` and a
   unique first-boot password for user `debian` — note the password down.

4. Flash it. This is board-specific — for the RK1 it is the Turing Pi BMC (`tpi` or the
   web UI), or a removable card. See [Turing RK1](docs/src/boards/turing-rk1.md).

Full walkthrough: [Getting started](docs/src/getting-started.md).

## Documentation

The docs are an mdBook under [`docs/`](docs/) — build it with `mdbook serve docs` (or
`mdbook build docs`), or read the chapter sources directly:

- [Introduction](docs/src/introduction.md)
- [Getting started](docs/src/getting-started.md) — prerequisites and your first build
- [Turing RK1](docs/src/boards/turing-rk1.md) — the shipped board, and how to flash it
- [Config model](docs/src/reference/config-model.md) — the axes and layer resolution
- [CLI](docs/src/reference/cli.md) — command reference
- [Adding a board](docs/src/contributing/adding-a-board.md)

## Layout

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
[`LICENSE`](LICENSE). Vendored third-party components (the Rockchip `rkbin`
firmware blobs, the boot and kernel-hook scripts, and the Debian archive keyring)
keep their own licenses; see [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
