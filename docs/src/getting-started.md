# Getting started

This gets you from a clone of the repo to a built image. It uses the shipped
`turing-rk1/forky` recipe as the running example; other boards build the same way
with their own recipe name. Flashing and board-specific notes live on each board's
page — for the RK1, see [Turing RK1](boards/turing-rk1.md).

> **Which track are you on?** This is the **shipped-recipe track** — `doctor` then
> `build`, for a recipe that already ships a committed lock (like `turing-rk1/forky`).
> To change what a shipped recipe builds, continue with
> [Adapting a shipped recipe](tutorials/adapting-a-recipe.md). Bringing up a *new* board,
> or authoring a patch, is the longer bring-up track: see
> [Adding a board](contributing/adding-a-board.md) and
> [Adding a patch](contributing/adding-a-patch.md).

The build is **rootless**: it uses no `sudo` and no loop devices. You only need root
to install host packages and (on some hosts) to enable unprivileged user namespaces
once.

## What you need

- **A Linux host, x86_64 or arm64.** An x86_64 desktop building the arm64 image is
  the common case and fully supported — it cross-builds under `qemu-user`. Debian
  and Ubuntu are the primary targets; Fedora and Arch work too (`doctor` knows their
  package names). macOS can run the read-only commands but cannot build.
- **A recent stable Rust toolchain**, installed via [rustup](https://rustup.rs).
- **`boot2deb` on your `PATH`.** From a clone of this repo:

  ```sh
  cd boot2deb
  cargo install --path crates/cli    # the crate is boot2deb-cli; the binary is boot2deb
  ```

  Every command on this site is written as `boot2deb …`, which is also how the tool
  writes its own hints — so anything it suggests can be pasted back. Developing from a
  checkout without installing? Prefix each command with `cargo run -p boot2deb-cli --`.
  Either way, run it from inside `boot2deb/` (or pass `--root <dir>`), since the config
  root defaults to the current directory.
- **Disk and time.** A cold build bootstraps a Debian rootfs, and — for a board that
  needs one — compiles a kernel and a bootloader. Budget a few GB of scratch space and
  tens of minutes the first time; later builds reuse cached trees. A board that compiles
  nothing (the C201) is much cheaper: it is a rootfs bootstrap and an image assembly.

## Let `doctor` find what's missing

Rather than hand-installing a package list, run `doctor`. It probes for every tool
the build needs and, for anything absent, prints the exact install command **for
your distro** — so you never guess a package name. `doctor` itself needs nothing but
Rust, so it is the first thing to run after cloning:

```sh
boot2deb doctor turing-rk1/forky
```

Run it bare — `boot2deb doctor` — and it checks the requirements every board shares
(user namespaces and the vendored apt trust anchors) without needing a recipe chosen yet.

It reports your host arch, the two cross answers, and one line per requirement:

```
host arch : x86_64
target    : turing-rk1/forky (arch arm64)
toolchain : cross — the build root carries a toolchain emitting arm64
execution : emulated — needs qemu-user binfmt for arm64 maintainer scripts and sandbox compiles

  ok      git                          /usr/bin/git
  ok      unprivileged user namespaces unshare --map-root-user --map-auto works
  MISSING qemu-aarch64-static          run target binaries under binfmt — sudo apt install qemu-user-static
  ...

result    : all required host tools present
```

Run the install lines it reports, then re-run `doctor` until it prints
`all required host tools present`. Because the list is generated from the build's own
requirements, it is always current — this page does not repeat the package names, so
there is nothing here to drift out of date.

The list is short, and that is the design rather than an omission. **Every compiler,
packaging tool and build dependency a build runs is a package of a provisioned Debian
root**, resolved from your build's own mirror list and sha256-pinned in that root's
manifest — so it is an input your lock names, not a fact about your machine. There is
no host `gcc`, no `make`, no `dpkg`, no `fakeroot` in the list, because none of them is
what compiles or archives anything. What remains is the handful of things no root can
carry:

| Group | What it covers | When |
| --- | --- | --- |
| Provisioned roots | unprivileged user namespaces with a subuid/subgid range — every root is bootstrapped and entered in-process, so this is the whole requirement | always |
| Sources | `git`, which clones your pinned trees and applies the patch series before there is a root for them to enter | only if the recipe compiles something |
| Build roots | an unprivileged overlay whose upper layer sits on the work dir's filesystem, which is how each compile root layers a stage's build dependencies | only if the recipe compiles something |
| Image assembly | `tar` and `cp`. No filesystem tooling — the rootfs ext4 is formatted and then scanned back in pure Rust; `e2fsck` is an optional independent cross-check when present, and the image's provenance records whether it ran | only if the recipe assembles an image |
| Emulation | `qemu-<arch>-static` + a registered binfmt handler, so the target's maintainer scripts run | only if the recipe assembles an image *and* this host cannot execute target binaries |

**`doctor` asks only for what *your recipe* will actually invoke**, so the table above
is a superset. `doctor turing-rk1/media-accel-forky` on an x86_64 host wants every row;
`doctor asus-c201/forky` wants no `git` and no overlay at all, because that board
installs Debian's kernel and boots its own firmware and so compiles nothing. That is
deliberate: a requirement you do not need is somewhere a requirement you *do* need can
hide.

Note that the last row's two conditions are both real, and each one alone would
over-ask. Emulation is about *running* target binaries, which only the image path does —
the rootfs runs the target's maintainer scripts, and the media-accel packages compile in
a target-arch sandbox. A bootloader-only build like `rk3576-generic/loader` compiles in
a **host-arch** root and executes nothing foreign, so it needs no qemu even though it
builds for arm64. And an arm64 host runs armhf binaries directly (its kernel is built
with `CONFIG_COMPAT=y`), so building the armhf C201 image there needs no `qemu-arm`
either. Any x86_64 host building an arm64 or armhf *image* needs both.

The target-arch sandbox is **not** a cross-only concern. Packages like `ffmpeg-rk` and
`librga2` are built inside a userland bootstrapped for the target *suite*, never on your
host, even when your host arch already matches the target. Their runtime `Depends` are
derived from the libraries present at build time, so building them against your host's
libraries would stamp your host's package names and versions into a `.deb` bound for a
Debian `forky` image. That sandbox runs entirely in-process through unprivileged user
namespaces — it needs no external sandbox tool — but it does run on every host, same-arch
included, so those namespaces are a hard requirement even when nothing is cross.

### The roots a build provisions

A build stands up as many as four Debian roots, each for one job, each bootstrapped and
entered in-process through unprivileged user namespaces. They live in your work dir and
`boot2deb clean --sandbox` reclaims them.

| Root | Architecture | What it holds | What runs in it |
| --- | --- | --- | --- |
| `sandbox/cross-<arch>-<suite>-<digest>/` | **host's** | a cross toolchain emitting the target's objects, plus each stage's build deps layered on (~800 MB) | the kernel, u-boot and out-of-tree module compiles — and the kernel's own `make bindeb-pkg`, which packages itself |
| `sandbox/build-<arch>-<suite>-<digest>/` | target's | `build-essential`, `dpkg-dev`, `debhelper` | the media-accel `.deb`s (`ffmpeg-rk`, `librga2`, MPP) |
| `sandbox/package-<arch>-<suite>-<digest>/` | **host's** | `dpkg` and `xz-utils` and nothing else (~130 MB) | archiving the u-boot and kmod `.deb`s, which boot2deb stages itself |
| the rootfs | target's | your image's solved package set | the image itself |

The two host-arch roots are host-arch on purpose. Neither compiling a freestanding
kernel nor archiving a staged tree needs to *link* against the target's libraries, so
both run natively — which is what keeps a multi-minute kernel build and a hundred-megabyte
`xz` off `qemu-user` entirely. The target-arch sandbox is the one that genuinely cannot
be: `dpkg-shlibdeps` derives each media-accel `.deb`'s runtime `Depends` from the
libraries present at build time.

Each root is provisioned for your build's suite — the image's, or for a bootloader-only
build the board's declared default — and each publishes a sha256-pinned manifest of its
own packages beside your image, so "what compiled this" and "what archived this" are
answered by name and version rather than by a `--version` line off your `PATH`. See
[Reproducibility](reference/reproducibility.md).

There is no `fakeroot` anywhere in boot2deb, in any root or on your host. Every root maps
you to uid 0, and uid 0 is what a Debian packaging tool actually wants: your staged tree
is already `root:root` where `dpkg-deb` archives it, and `dpkg-buildpackage` picks no
gain-root command at all. Nothing is faked because nothing needs to be.

### The user-namespace check (common blocker on Ubuntu 24.04)

The rootless rootfs bootstrap, the sandbox, and the ext4 image staging all need
**unprivileged user namespaces** with a subuid/subgid range for your user, which some
hosts disable by default. `doctor` tests this by actually creating one (with the
subuid mapping), and if it fails it prints the fix for your host. The usual cases:

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
- Your user needs a subuid/subgid range (usually present by default):
  ```sh
  sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER
  ```

`sysctl -w` lasts until reboot; drop the same line in `/etc/sysctl.d/` to make it
persist.

On a build that assembles an image for an architecture your host cannot execute,
`doctor` also checks that the `qemu-<arch>` **binfmt handler is registered and enabled
with the `F` (fix-binary) flag** — the rootfs bootstrap relies on it. Installing
`qemu-user-static` (with `binfmt-support` / systemd's binfmt) normally registers this;
`doctor` warns if the flag is missing.

### The overlay check

Every stage that compiles gets a **build root**: the shared base plus that stage's own
build-dependencies, layered on with an unprivileged overlay and discarded afterwards.
`doctor` probes whether your host can establish one, and prints the directory it probed.

That is why the overlay is a requirement of *compiling* rather than of every build. A
board that installs Debian's kernel and boots its own firmware layers nothing and needs
only user namespaces — and so does a rebuild whose artifacts all restore from the cache,
which never stands a build root up at all.

The probe is pointed at the **work dir's** filesystem, not at `/tmp`, because that is
where the overlay's upper layer goes and the two can answer differently. An unprivileged
overlay records its whiteouts in `user.*` extended attributes: every on-disk filesystem
(ext4, xfs, btrfs) holds them, but tmpfs only gained them in Linux 6.6. So a host with a
tmpfs `/tmp` on an older kernel would fail a `/tmp` probe and still build fine with its
work dir on disk. If you build with `--work-dir`, pass the same path to `doctor` so it
asks about the filesystem you will actually use.

The mount itself needs Linux 5.11 or later, which is where overlay in a user namespace
arrived.

## Build

With `doctor` green:

```sh
boot2deb build turing-rk1/forky
```

This resolves the recipe's committed lockfile and runs the pipeline end to end. For
`turing-rk1/forky` that is: compile the kernel and u-boot, bootstrap the Debian rootfs,
and assemble a bootable disk image. **A recipe runs only the stages it has** —
`turing-rk1/media-accel-forky` adds the Rockchip media userspace and ffmpeg on top of
those, while `build asus-c201/forky` compiles nothing at all, so it is a rootfs bootstrap
and an image assembly and nothing else.

The build reads only the lock, so it consults no network for its pins and is reproducible
from what is committed. A patch series, where a recipe has one, is fetched automatically
at its pinned commit if the config root's sibling `../patches` checkout is not already
present — you do not need to clone it separately. That holds for both patch axes: the
repo comes from the lock's own pin, so a u-boot-only recipe such as
`rk3576-generic/loader` fetches its series the same way a kernel recipe does.

The rootfs bootstrap is content-cached, so a rebuild whose solved package set is
unchanged skips the multi-minute bootstrap. To force a clean rootfs, add
`--refresh-rootfs`. To build a single stage, pass `--stage`
(`kernel`, `dtb`, `kmod`, `uboot`, `userspace`, `ffmpeg`, `rootfs`, `image`) — see the
[CLI reference](reference/cli.md).

### What you get

Artifacts land under the recipe's work dir, `build/turing-rk1/forky/artifacts/`:

- **`turing-rk1-forky.img.xz`** — the compressed bootable image.
- **`turing-rk1-forky.provenance.toml`** — exactly what went into the image: the
  resolved pins, package count, toolchain identity, and the first-boot credential.

Every artifact is named for the whole build point — device and recipe
(`turing-rk1/forky` → `turing-rk1-forky`) — so several recipes can share one
`--out-dir`, and an image copied to a flashing host still says what it is.

The build prints the exact paths on its final lines, including the credential:

```
compressed    : .../build/turing-rk1/forky/artifacts/turing-rk1-forky.img.xz
first-boot pw : <generated>  (user debian, expired — change at first login)
provenance    : .../build/turing-rk1/forky/artifacts/turing-rk1-forky.provenance.toml
```

**Note the first-boot password down.** It is unique per image, shown once here, and
stored only in the provenance file — it exists nowhere on the running system in
recoverable form.

Next: flash the image. That step is board-specific — for the RK1, see
[Turing RK1](boards/turing-rk1.md).
