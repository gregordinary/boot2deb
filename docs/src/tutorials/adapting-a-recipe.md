# Adapting a shipped recipe

Every shipped recipe is a *point* across the build axes — device, kernel, u-boot series,
suite, features, layout — and adapting one means naming a different point. Most of what
people want from an image is reachable without writing a file at all, and the rest is a
few lines of TOML in a directory of your own.

This tutorial works up the four levels of change, cheapest first. It assumes you have
already built a shipped recipe once ([Getting started](../getting-started.md)).

| What you want to change | How | What it writes |
| --- | --- | --- |
| The artifact's geometry or where it lands (`--layout`, `--image-size`, `--out-dir`) | flags on `build` | nothing — the lock is untouched |
| Which features are in the image | `update <recipe> --feature …`, then build the variant reference | a variant lock beside the recipe |
| Suite, kernel, u-boot series, board profile, locale, timezone, keymap | a recipe file of your own | your recipe plus its lock |
| A hardware fact of the board | a device file, usually `extends` another | your device plus a recipe |

## Where your files go

Put anything you author in a directory of your own and pass `--overlay`:

```sh
mkdir -p ~/my-boards/recipes/asus-c201
boot2deb --overlay ~/my-boards resolve asus-c201/forky
```

An overlay holds the same `devices/ socs/ kernels/ features/ recipes/` structure as the
shipped root, wins over it name-for-name, and takes the locks that `update` writes — so
there is nothing to fork and nothing of yours to rebase when the shipped tree moves. See
[Overlays](../reference/overlays.md). The rest of this page assumes `--overlay ~/my-boards`
wherever you author something; drop it if you are contributing the result back in-tree.

## Level 1: change the artifact, not the build

`build` re-reads the lock for every pinned input, but the image's *shape* is not a pinned
input, so two axes are overridable at build time:

```sh
# A bootloader-only image plus a separate rootfs image, for a two-medium install.
boot2deb build turing-rk1/forky --layout split

# A bigger artifact. The rootfs grows to fill its medium on first boot regardless, so
# this bounds the file you flash, not the installed system.
boot2deb build turing-rk1/forky --image-size 4G

# Keep the raw image beside the .xz, and put artifacts somewhere else.
boot2deb build turing-rk1/forky --keep-raw --out-dir /mnt/scratch
```

Artifacts are named for the whole build point — device and recipe
(`turing-rk1/forky` → `turing-rk1-forky.img.xz`, `turing-rk1-forky-rootfs.tar`,
`turing-rk1-forky-idbloader.img`) — so several recipes can share one `--out-dir`
without one build's rootfs or bootloader being folded into another's image.

Nothing here is recorded as a new point: the lock, the pins, and the support claim all
still describe the recipe you named. `--stage` narrows the run to one stage, which is how
you iterate on the image assembly without recompiling a kernel — see the
[CLI reference](../reference/cli.md#build).

## Level 2: compose features

Features are the one axis you can select without authoring anything. `update` pins the
selection as a *variant* of the recipe, and the variant is then a build reference like any
other:

```sh
boot2deb list-features
boot2deb update turing-rk1/forky --feature media-accel-rockchip --feature jellyfin
boot2deb build  turing-rk1/forky+media-accel-rockchip+jellyfin
```

The variant gets its own lock, its own solved package manifest, and its own work directory
and image identity, so it never lands on the recipe's artifacts. Three things worth
knowing before you rely on it:

- **The selection replaces the recipe's `features` list**, it does not add to it. Name
  everything you want.
- **Order matters and is preserved** — kconfig fragments and patch series compose in
  selection order, so `a+b` and `b+a` are two different builds and two different
  references.
- **A variant carries no support claim.** The claim belongs to the recipe; a different
  feature set is a different build. Variants appear in neither `list-recipes` nor the
  support matrix.

Reference: [A feature selection is a build
point](../reference/config-model.md#a-feature-selection-is-a-build-point-not-a-new-recipe).

## Level 3: author your own recipe

Any axis that is *not* a feature — the suite, the kernel definition, the u-boot series,
the depthcharge board profile, the locale, timezone, and keymap — is pinned by the recipe
file, so changing one means a recipe of your own. That is deliberately cheap: a recipe
states its deltas from the device's defaults and nothing else.

This is also what `resolve` tells you to do. Preview a choice with the matching flag and
it prints the recipe to write, with the keys already filled in:

```sh
boot2deb resolve asus-c201/forky --suite trixie --keymap de
```

The worked example is the ASUS C201 on `trixie`, localized for Germany.
`~/my-boards/recipes/asus-c201/trixie-de.toml`:

```toml
device   = "asus-c201"
suite    = "trixie"
locale   = "de_DE.UTF-8"
timezone = "Europe/Berlin"
keymap   = "de"
```

Five lines is the whole file. Everything else — the kernel (Debian's own `linux-image-armmp`
for this board), the boot method, the layout, the image size — comes from
`devices/asus-c201.toml`. Resolve it to see the point in full, including which values came
from where:

```sh
boot2deb --overlay ~/my-boards resolve asus-c201/trixie-de
```

```
device       : asus-c201 — ASUS Chromebook C201 (RK3288, google,veyron-speedy)
kernel       : debian-armmp (distro-package)
suite        : trixie
locale       : de_DE.UTF-8 (generated: de_DE.UTF-8, en_US.UTF-8, en_GB.UTF-8, fr_FR.UTF-8, ...)
timezone     : Europe/Berlin
keymap       : de [pc105]
board profile: speedy
...
```

`resolve` also runs the coherence preflight — image geometry, fragment existence, feature
compatibility, apt keyrings — so a green resolve means the point is buildable, not merely
parseable. Then pin it and build:

```sh
boot2deb --overlay ~/my-boards update asus-c201/trixie-de
boot2deb --overlay ~/my-boards build  asus-c201/trixie-de
```

`update` writes `trixie-de.lock` next to your recipe, inside your overlay. It needs no
`--kernel-ref` here because this board installs Debian's kernel: there is no git ref to
resolve, and the exact package version is pinned by the solved package manifest instead.
A board that compiles a kernel wants `--kernel-ref <tag>` on its first update, after which
`update` inherits the previous ref.

**Which locale keys to set where** is its own topic — the layered defaults, and what each
key does to the running system, are on [Locale, timezone, and
keyboard](../localization.md).

## Level 4: adapt the board itself

If what you need to change is a hardware fact — a different DTB, different DRAM timing, a
peripheral enabled for bring-up — that is the device layer, not the recipe. A board that is
another board with one difference uses `extends`:

```toml
# ~/my-boards/devices/my-rk1-variant.toml
extends     = "turing-rk1"
description = "Turing RK1 with a different DDR fitting"
hostname    = "rk1-variant"

[rkbin]
tpl = "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin"
```

`extends` inherits the parent device's keys *and* its `overlay/` file tree, so the parent's
driver tuning, units, and keymaps reach your image, and you override any single file by
shipping your own copy at the same path. Arrays replace rather than append across the
merge, so restate any list you extend. Details:
[A variant board extends another](../reference/config-model.md#a-variant-board-extends-another).

Reach for a variant device only when the difference needs a device-layer field — a device
tree, a DTB name, the DRAM blob. A capability whose whole expression is packages, kernel
config, and a patch series is a feature instead, and features compose a-la-carte where a
variant device does not.

Beyond that — a board that is genuinely new, a SoC that is not here yet, a device tree that
is not upstream — is the bring-up track: [Adding a board](../contributing/adding-a-board.md),
which starts with `boot2deb new-device` scaffolding the files for you.

## When the change is worth a claim

A recipe is the unit that carries a `[support]` claim, and a claim is a statement about
hardware. If you have booted your adapted image and want to say so — in your own overlay or
in a contribution — see [Authoring a recipe](authoring-a-recipe.md).
