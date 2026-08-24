# Authoring a recipe

A recipe is a **name for a buildable point**, and little more: it names a device, states
whichever axes differ from that device's defaults, and optionally declares what the point
has been taken through. Everything else — the hardware facts, the kernel, the boot chain —
belongs to the layers underneath it.

This tutorial is about the recipe file itself. Bringing up the *board* under it is
[Adding a board](../contributing/adding-a-board.md); changing an existing point without
authoring anything is [Adapting a shipped recipe](adapting-a-recipe.md).

## First: does this need to be a recipe?

Two things that look like recipes are not:

- **A different feature selection** is a build point, not a file. `update <recipe>
  --feature a --feature b` pins it as a variant with its own lock, and `build
  <recipe>+a+b` builds it. The legal selections grow exponentially in the number of
  features, and almost none of them are anybody's curated point.
- **A different image geometry** is a `build` flag (`--layout`, `--image-size`).

Author a recipe when a point is worth *claiming* — something you have booted, or intend to
support, or want to hand someone else by name. Use a variant for everything else.

## The file

`recipes/<device>/<leaf>.toml`. Recipes group under their device's folder, so a board's
whole matrix sits together, and the reference you build is that path without the extension:
`recipes/turing-rk1/media-accel-forky.toml` builds as `turing-rk1/media-accel-forky`.

Only `device` is required. Every other axis falls back to the device layer, so state a
field when you mean to differ from the board's default — an omitted axis reads as
"whatever this board does", which stays correct when the board's default moves. The full
set of axes a recipe may pin:

```toml
device       = "<device>"        # required
kernel       = "<kernel-id>"     # omit -> device default_kernel
uboot_series = "<series>"        # omit -> device default_uboot_series
suite        = "forky"           # omit -> device default_suite
features     = []                # omit -> a plain base image
layout       = "combined"        # omit -> device default_layout
image_size   = "2G"              # omit -> device image_size
locale       = "de_DE.UTF-8"     # omit -> base locale
locales_generate = []            # omit -> base locales_generate (replaces, not adds)
timezone     = "Europe/Berlin"   # omit -> base timezone
keymap       = "de"              # omit -> device keymap
```

Real recipes are much shorter than that, because most of it is the board's default already.
`recipes/turing-rk1/forky.toml` states five fields; the C201 on `trixie` with German
localization states five.

`kernel` and `uboot_series` must name one of the device's `supported_kernels` /
`supported_uboot_series` — a recipe selects among what the board declares it can run, it
does not widen it. `list-kernels` and `list-features` enumerate the valid values.

### A recipe whose deliverable is a bootloader

Not every recipe produces a disk image. `deliverable = "uboot"` produces a bootloader and
nothing else — for an RK3576 board, the maskrom-streamable images from `--stage uboot`.
Such a recipe names no suite, kernel, layout, or image size; resolution ignores those axes
and the lock records no rootfs:

```toml
device       = "rk3576-generic"
deliverable  = "uboot"
uboot_series = "rk3576-util"
```

That is the whole file for a recovery-and-bring-up tool that works on any RK3576 board. See
[The bootloader is its own axis](../reference/config-model.md#the-bootloader-is-its-own-axis)
and [RK3576 u-boot images](../reference/rk3576-uboot-images.md).

## Naming the leaf

The leaf drops the device prefix its folder already carries, and names the axis that makes
the point distinct. The shipped tree uses four conventions, in rough order of how often
they come up:

| Leaf | Names | Example |
| --- | --- | --- |
| the suite | a plain image on that Debian release | `turing-rk1/forky`, `asus-c201/trixie` |
| a capability, plus the suite where the board ships more than one | what the image can do | `turing-rk1/media-accel-forky`, `h96-max-m9/media-accel` |
| the deliverable | a `deliverable = "uboot"` tool | `rk3576-generic/loader`, `h96-max-m9/util` |
| the product | an image built around one application | `turing-rk1/jellyfin` |

Do not put a version in a leaf name if the version is already pinned elsewhere: the lock
holds the exact tag, and a leaf named after it goes stale on the next bump. A leaf naming a
*kernel generation* the board supports alongside another is different, and reasonable
(`asus-c201/mainline-forky` is the C201 on a compiled mainline kernel rather than Debian's).

## The support claim

A recipe may carry the one thing no lock can know — whether a human booted the result:

```toml
[support]
status = "validated"     # validated | expected | experimental
date   = "2026-07-16"
```

| Status | What it asserts |
| --- | --- |
| `validated` | An image built from this recipe booted on the hardware. |
| `expected` | Derived from a validated sibling, differing only along an axis not expected to change the outcome; never built, or built and never booted. |
| `experimental` | Under active bring-up. It may not build. |

Three properties make the claim mean something:

- **It is per recipe, not per device**, because it varies within a device: one build point
  can be booted while another — a different kernel, suite, or feature set — was never built.
- **It is per pin.** The date is when the claim was last established, and the generated
  [support matrix](../reference/support-matrix.md) sets it beside the exact pins the lock
  records. Re-pinning under a `validated` claim retires the evidence, and `update` says so
  at the moment both locks exist to compare. A claim cannot be re-dated for pins that moved
  — it has to be re-earned by booting an image.
- **Absent means no claim made**, not a fourth status. That is the honest state for a
  recipe you authored against your own board, and `support-matrix` reports unclaimed
  recipes separately rather than dropping them.

Every recipe boot2deb *ships* declares a claim, and a test enforces it.

## Bring the recipe up

Whether the recipe lives in your `--overlay` or in-tree, the sequence is the same, and each
step fails with a typed error before any compile starts:

```sh
# Is it a coherent build point? Also runs the geometry / fragment / keyring preflight.
cargo run -p boot2deb-cli -- resolve <recipe>

# Is the host equipped to build it? Prints install commands for your distro.
cargo run -p boot2deb-cli -- doctor <recipe>

# Resolve upstream refs + hash blobs into the lock. The only command that touches
# the network for pins; --kernel-ref is required on the first update of a recipe
# that compiles a kernel.
cargo run -p boot2deb-cli -- update <recipe> --kernel-ref <tag>

# Do the patch series apply, and does the kernel .config generate?
cargo run -p boot2deb-cli -- verify-patches <recipe>
cargo run -p boot2deb-cli -- verify-config  <recipe>

# Build.
cargo run -p boot2deb-cli -- build <recipe>
```

`update` writes `recipes/<device>/<leaf>.lock` beside the recipe — into your overlay when
that is where the recipe lives. **Commit the lock.** It is what makes the point
reproducible: `build` reads only the lock, so it consults no network for its pins.

## If you are contributing it in-tree

Three things beyond the file itself:

1. **Declare a `[support]` claim.** A shipped recipe without one fails the test that
   enforces it.
2. **Regenerate the support matrix**, which is generated from the recipes and their locks
   and is compared by a test:
   ```sh
   cargo run -p boot2deb-cli -- support-matrix --markdown > docs/src/reference/support-matrix.md
   ```
3. **Check the pins are durable** — a lock naming an unpushed commit builds for you and
   nobody else:
   ```sh
   cargo run -p boot2deb-cli -- verify-sources <recipe>
   ```

If the recipe is the board's first, give the board a page under
[Boards](../boards/turing-rk1.md) as well: flashing is inherently per-board, and it is the
one thing no config file can state.
