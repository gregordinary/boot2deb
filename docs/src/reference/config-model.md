# Config model

A build is a single point across five axes:

**device × kernel × suite × profile × layout**

- **device** — the target hardware. It resolves through a layered hardware stack (see
  below).
- **kernel** — an orthogonal axis that owns its version-coupled refs, `.config`
  fragments, and patch profile. A device declares which kernels it supports and a
  default.
- **suite** — the Debian suite (e.g. `forky`, `sid`).
- **profile** — the patch profile applied to the sources (e.g. `rk3588-accel`). Patch
  profiles live in a separate `patches` repo, not in this one.
- **layout** — how the disk image is packaged: `combined` (one whole-disk image with
  the bootloader in the raw gap) or `split` (separate bootloader and rootfs images for
  a two-medium install).

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

## Recipes and the lock

A **recipe** (`recipes/<recipe>.toml`) pins one point across the five axes — it names
the device, kernel, suite, profile, and layout. Its **lock** (`recipes/<recipe>.lock`)
holds the exact resolved pins: commit hashes for every source, blob content hashes, and
the solved rootfs manifest digest.

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
