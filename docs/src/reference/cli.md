# CLI

The binary is `boot2deb`; during development run it with `cargo run -p boot2deb-cli --`.
It defaults `--root .`, so run it from inside `boot2deb/` (or pass `--root`).

The two commands that split reproducibility from upstream are `update` (the only one
that consults the network) and `build` (reads only the lock). See
[Config model](config-model.md) for that split.

## Inspection

```sh
cargo run -p boot2deb-cli -- list-devices
cargo run -p boot2deb-cli -- list-recipes
cargo run -p boot2deb-cli -- resolve turing-rk1-forky
cargo run -p boot2deb-cli -- resolve turing-rk1 --suite sid --layout split
cargo run -p boot2deb-cli -- doctor turing-rk1-forky
```

- **`resolve`** prints the fully merged build point without building. Image axes
  (`--layout`, `--suite`, `--image-size`) can be overridden on the command line.
- **`doctor`** reports the host's tool-presence preflight for a target and, for anything
  missing, the exact per-distro install command. See [Getting started](../getting-started.md).

## update

```sh
cargo run -p boot2deb-cli -- update turing-rk1-forky --kernel-ref v7.1.1
```

Resolves upstream refs to commits and hashes the vendored blobs, writing
`recipes/<recipe>.lock`. This is the **only** command that consults upstream; `build`
reads only the lock, so a build is reproducible from its committed pins.

## build

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky
```

Builds the recipe from its lock: compiles the kernel, u-boot, userspace, and ffmpeg,
bootstraps the rootfs, and writes the bootable disk image. Notable flags:

- **`--stage <node>`** runs a single node — `kernel`, `uboot`, `userspace`, `ffmpeg`,
  `rootfs`, or `image`; the default builds everything. A `--stage uboot` run also emits
  a standalone, directly-flashable `<device>-boot.img` (see below).
- **`--layout combined|split`** overrides the image packaging. `combined` is one
  whole-disk image; `split` emits a bootloader-only image and a separate rootfs image
  for a two-medium install. This is lock-independent — it changes only how the image is
  packaged, not any pinned source.
- **`--refresh-rootfs`** forces a clean rootfs bootstrap instead of restoring the
  content cache.

The rootfs stage is content-cached: a cheap `mmdebstrap --simulate` solve keys a store,
so a rebuild whose *solved* package set is unchanged restores the bootstrapped tree
instead of re-running the multi-minute bootstrap. Because the key is the solved set, a
moved mirror resolves new versions and rebuilds automatically — a cache hit is never
stale. The unique per-image first-boot password is applied on restore, not cached, so
every image still gets its own credential.

### Standalone bootloader image

`build <recipe> --stage uboot` writes `<device>-boot.img` next to the raw `idbloader.img`
and `u-boot.itb`: a small, GPT-less image holding just the bootloader at its offsets. It
needs no rootfs, so you can produce a flashable eMMC/SPI bootloader image without building
a whole OS. The `split` layout emits the same image as part of a full build. See
[Turing RK1](../boards/turing-rk1.md) for the eMMC-plus-NVMe workflow this serves.

## Patch and config verification

```sh
# Dry-run the locked patch series against a kernel checkout (git am --3way), hard-erroring
# on the first patch that does not apply.
cargo run -p boot2deb-cli -- verify-patches turing-rk1-forky \
  --kernel-path /path/to/linux --patches-path ../patches

# Generate the kernel .config (base defconfig + fragments) on a patched tree and report
# the merge. With --reference-config, additionally assert byte-identical CONFIG_* parity
# against a reference config.
cargo run -p boot2deb-cli -- verify-config turing-rk1-forky \
  --kernel-path /path/to/patched-linux

# Import a patch into a profile: fetch (a patchwork/mbox URL, a file, or stdin), normalize
# to canonical git am-ready mbox, slot it into a scope, and -- with --verify-tree -- dry-run
# git am-verify the resulting series (rolling back on failure).
cargo run -p boot2deb-cli -- patch import https://patchwork.kernel.org/patch/NNN/mbox/ \
  --profile rk3588-accel --scope kernel --verify-tree /path/to/linux
```

## Rebuild planning and cleanup

```sh
# Explain, offline, whether the next build reuses or rebuilds each compile node's source
# tree -- and which pinned input changed if it will rebuild.
cargo run -p boot2deb-cli -- why-rebuild turing-rk1-forky

# Remove a recipe's build scratch to reclaim disk or force a clean rebuild. --dry-run
# previews; --cache / --sandbox clean only that subtree.
cargo run -p boot2deb-cli -- clean turing-rk1-forky --dry-run
```

`why-rebuild` reads the lock and each compile node's signature stamp and reports, per node,
whether the next `build` reuses or rebuilds the cloned-and-patched tree, naming the pinned
input that moved when it will rebuild. It runs no build and touches no network.
