# Getting started

This gets you from a clone of the repo to a built image. It uses the shipped
`turing-rk1-forky` recipe as the running example; other boards build the same way
with their own recipe name. Flashing and board-specific notes live on each board's
page — for the RK1, see [Turing RK1](boards/turing-rk1.md).

The build is **rootless**: it uses no `sudo` and no loop devices. You only need root
to install host packages and (on some hosts) to enable unprivileged user namespaces
once.

## What you need

- **A Linux host, x86_64 or arm64.** An x86_64 desktop building the arm64 image is
  the common case and fully supported — it cross-builds under `qemu-user`. Debian
  and Ubuntu are the primary targets; Fedora and Arch work too (`doctor` knows their
  package names). macOS can run the read-only commands but cannot build.
- **A recent stable Rust toolchain**, installed via [rustup](https://rustup.rs).
- **Disk and time.** A cold build compiles the kernel and u-boot and bootstraps a
  Debian rootfs — budget a few GB of scratch space and tens of minutes the first
  time. Later builds reuse cached trees.

## Let `doctor` find what's missing

Rather than hand-installing a package list, run `doctor`. It probes for every tool
the build needs and, for anything absent, prints the exact install command **for
your distro** — so you never guess a package name. `doctor` itself needs nothing but
Rust, so it is the first thing to run after cloning:

```sh
cd boot2deb
cargo run -p boot2deb-cli -- doctor turing-rk1-forky
```

It reports your host arch, whether the build is cross-arch, and one line per
requirement:

```
host arch : x86_64
target    : turing-rk1-forky (arch arm64)
cross     : yes — needs qemu-user binfmt for arm64 maintainer scripts/compiles

  ok      git                          /usr/bin/git
  MISSING mmdebstrap                   rootfs bootstrap — sudo apt install mmdebstrap
  MISSING qemu-aarch64-static          run target binaries under binfmt — sudo apt install qemu-user-static
  ...

result    : all required host tools present
```

Run the install lines it reports, then re-run `doctor` until it prints
`all required host tools present`. Because the list is generated from the build's own
requirements, it is always current — this page does not repeat the package names, so
there is nothing here to drift out of date.

For orientation, the checks fall into a few groups:

| Group | What it covers | When |
| --- | --- | --- |
| Compile toolchain | `git`, `make`, `bc`, `flex`, `bison`, `libssl`, and a C compiler | always |
| Rootfs bootstrap | `mmdebstrap` + unprivileged user namespaces | always |
| Packaging / apt repo | `dpkg-deb`, `dpkg-scanpackages`, `apt-ftparchive`, `sha256sum` | always |
| Image assembly | `tune2fs` (the ext4 filesystem itself is built in pure Rust) | always |
| Cross toolchain | the `<triple>gcc` cross compiler (e.g. `gcc-aarch64-linux-gnu`) | cross only |
| Emulation | `bwrap`, `qemu-<arch>-static`, and a registered binfmt handler | cross only |

The "cross only" rows apply when your host arch differs from the target — i.e. any
x86_64 host building an arm64 image. An arm64 host builds it natively and skips them.

### The user-namespace check (common blocker on Ubuntu 24.04)

The rootless rootfs bootstrap and sandbox both need **unprivileged user namespaces**,
which some hosts disable by default. `doctor` tests this by actually creating one, and
if it fails it prints the fix for your host. The usual cases:

- **Ubuntu 24.04+** ships an AppArmor restriction on by default:
  ```sh
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
  ```
- **Debian** with namespaces disabled:
  ```sh
  sudo sysctl -w kernel.unprivileged_userns_clone=1
  ```
- Either way, `kernel.max_user_namespaces` (or `user.max_user_namespaces`) must be
  greater than 0.

`sysctl -w` lasts until reboot; drop the same line in `/etc/sysctl.d/` to make it
persist.

On a cross build `doctor` also checks that the `qemu-<arch>` **binfmt handler is
registered and enabled with the `F` (fix-binary) flag** — the sandbox relies on it.
Installing `qemu-user-static` (with `binfmt-support` / systemd's binfmt) normally
registers this; `doctor` warns if the flag is missing.

## Build

With `doctor` green:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky
```

This resolves the recipe's committed lockfile and runs the pipeline end to end:
compile the kernel and u-boot, build the media-accel userspace and ffmpeg, bootstrap
the Debian rootfs, and assemble a bootable disk image. The build reads only the lock,
so it consults no network for its pins and is reproducible from what is committed. The
patch series is fetched automatically at its pinned commit if a `../patches` checkout
is not already present — you do not need to clone it separately.

The rootfs bootstrap is content-cached, so a rebuild whose solved package set is
unchanged skips the multi-minute bootstrap. To force a clean rootfs, add
`--refresh-rootfs`. To build a single stage, pass `--stage`
(`kernel`, `uboot`, `userspace`, `ffmpeg`, `rootfs`, `image`) — see the
[CLI reference](reference/cli.md).

### What you get

Artifacts land under the recipe's work dir, `build/turing-rk1-forky/artifacts/`:

- **`turing-rk1.img.xz`** — the compressed bootable image (the file is named after the
  device, not the recipe).
- **`turing-rk1-forky.provenance.toml`** — exactly what went into the image: the
  resolved pins, package count, toolchain identity, and the first-boot credential.

The build prints the exact paths on its final lines, including the credential:

```
compressed    : .../build/turing-rk1-forky/artifacts/turing-rk1.img.xz
first-boot pw : <generated>  (user debian, expired — change at first login)
provenance    : .../build/turing-rk1-forky/artifacts/turing-rk1-forky.provenance.toml
```

**Note the first-boot password down.** It is unique per image, shown once here, and
stored only in the provenance file — it exists nowhere on the running system in
recoverable form.

Next: flash the image. That step is board-specific — for the RK1, see
[Turing RK1](boards/turing-rk1.md).
