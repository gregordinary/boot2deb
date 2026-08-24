# CLI

The binary is `boot2deb`; during development run it with `cargo run -p boot2deb-cli --`.
It defaults `--root .`, so run it from inside `boot2deb/` (or pass `--root`).

Three global flags apply to every command: `--root <dir>` (the config root),
`--overlay <dir>` (an out-of-tree config overlay, repeatable — see
[Overlays](overlays.md)), and `--json` (machine-readable output).

`--root` moves everything, not just where config is read from. A run's durable
state is anchored to the config root: the build scratch (`<root>/build/<recipe>`),
the artifact, patches, extra-deb, and verify-tree caches (`<root>/cache/...`), and
the default `patches` checkout (the root's sibling `../patches`). So `--root
boot2deb why-rebuild turing-rk1/forky` from the parent directory inspects the same
trees a run from inside `boot2deb/` builds into. An explicit `--work-dir` or
`--patches-path` is taken as given, relative to the current directory.

Under `--json`, the `list-*` commands print one JSON array (unreadable entries
become `{"name", "error"}` objects), `resolve` prints the fully resolved build as
one JSON document, and `build` streams NDJSON — one JSON object per line, tagged
by its `event` field (`step_started`, `progress`, `log`, `artifact`,
`step_finished`, `error`), with every produced artifact's path carried by an
`artifact` event. Errors are still plain text on stderr, and the exit code is the
result either way. Other commands print their human form regardless.

The two commands that split reproducibility from upstream are `update` (the only one
that consults the network) and `build` (reads only the lock). See
[Config model](config-model.md) for that split.

## Inspection

```sh
cargo run -p boot2deb-cli -- list-devices
cargo run -p boot2deb-cli -- list-recipes
cargo run -p boot2deb-cli -- list-kernels
cargo run -p boot2deb-cli -- list-features
cargo run -p boot2deb-cli -- list-kmods
cargo run -p boot2deb-cli -- support-matrix
cargo run -p boot2deb-cli -- resolve turing-rk1/forky
cargo run -p boot2deb-cli -- resolve turing-rk1 --suite sid --layout split
cargo run -p boot2deb-cli -- doctor turing-rk1/forky
```

- **`list-devices` / `list-recipes`** enumerate the buildable targets; `list-recipes`
  shows each recipe's support claim and flags any recipe with no committed lock as
  not-yet-buildable (run `update`).
- **`support-matrix`** prints that claim beside the exact pins the recipe's lock
  records — board, suite, kernel, patch series — so "which patch series worked with
  which kernel, on what board" is answerable without decoding a SHA. `--markdown`
  emits [the docs page](support-matrix.md) verbatim; regenerate it after changing a
  claim or re-pinning a lock, and a test fails if the committed page is stale.
- **`list-kernels` / `list-features`** enumerate the valid values for the `--kernel`
  and `--feature` overrides — name, version/compatibility, and (for kernels) the patch
  series — so the override knobs are discoverable without reading the TOML tree.
- **`list-kmods`** enumerates the out-of-tree kernel-module sets a device's
  `device_kmods` may name, with the driver ref each tracks and the modules it ships.
  Unlike the two above this is not an override: it is what a new board consults to find
  out whether the driver for its Wi-Fi part is already declared, before writing a second
  declaration of it.
- **`resolve`** prints the fully merged build point without building, and runs the same
  local `preflight_config` coherence check the build does (geometry, fragment-file
  existence, feature compatibility, apt keyrings). Selectable axes (`--kernel`,
  `--suite`, `--feature`, `--layout`, `--boot-method`, `--board`, `--image-size`,
  `--locale`, `--locale-gen`, `--timezone`, `--keymap`) can be overridden on the command
  line.
- **`doctor`** reports the host's tool-presence preflight for a target and, for anything
  missing, the exact per-distro install command. It asks only for what *that build* will
  invoke: a board that installs Debian's kernel and boots its own firmware compiles
  nothing, so it is not told to install a cross compiler — which keeps a genuinely
  missing tool from getting lost among requirements that do not apply. See
  [Getting started](../getting-started.md).

## Scaffolding

```sh
# Interactive on a terminal: menus over the valid SoC / boot-method / kernel / feature
# choices, then writes devices/<name>.toml + recipes/<name>/<suite>.toml.
cargo run -p boot2deb-cli -- new-device my-board

# Scriptable: take every value from flags (required: --soc), no prompts.
cargo run -p boot2deb-cli -- new-device my-board --soc rk3588 \
  --feature media-accel-rockchip --non-interactive

# Scaffold into your own overlay tree instead of the shipped root:
cargo run -p boot2deb-cli -- --overlay ~/my-boards new-device my-board --soc rk3588
```

**`new-device`** generates a device (and, unless `--no-recipe`, a matching recipe) from
the typed model. It offers only valid choices — the closed `Soc`/`BootMethod`/`Layout`
enums, the kernels whose `supported_socs` include the chosen SoC, and the features
compatible with the SoC/arch — fills every derivable value, and leaves the four
values it cannot validate (`uboot_defconfig`, `kernel_dtb`, and the `[rkbin]`
`atf`/`tpl` blobs) as best-effort suggestions marked `# TODO:`. It writes into the
highest-precedence `--overlay` when one is given (the third-party path), else the
primary root, then resolve-checks the result and prints exactly which values you still
have to research. It refuses to overwrite an existing file without `--force`.

The generated files resolve immediately (proving the layer composition); the `# TODO:`
values are the ones that fail *late* — at the u-boot or kernel build — if left wrong,
so verify them before `update`/`build`. See [Adding a board](../contributing/adding-a-board.md).

## update

```sh
cargo run -p boot2deb-cli -- update turing-rk1/forky --kernel-ref v7.1.1
```

Resolves upstream refs to commits and hashes the vendored blobs, writing
`recipes/<device>/<leaf>.lock`. This is the **only** command that consults upstream; `build`
reads only the lock, so a build is reproducible from its committed pins.

- **`--feature <name>`**, repeatable, pins a [feature
  selection](config-model.md#a-feature-selection-is-a-build-point-not-a-new-recipe)
  as a *variant* of the recipe — everything but the features comes from the recipe,
  and the lock lands beside it as `<leaf>+<feature>...lock`. A variant's first
  `update` inherits the recipe's pins, so it needs no `--kernel-ref`.

```sh
cargo run -p boot2deb-cli -- update turing-rk1/forky --feature media-accel-rockchip --feature jellyfin
# wrote recipes/turing-rk1/forky+media-accel-rockchip+jellyfin.lock
```

## build

```sh
cargo run -p boot2deb-cli -- build turing-rk1/forky
```

Builds the recipe from its lock: compiles the kernel, u-boot, userspace, and ffmpeg,
bootstraps the rootfs, and writes the bootable disk image. Notable flags:

- **`--feature <name>`**, repeatable, selects which lock to build — the one `update
  --feature` pinned. It does not re-resolve one, so a selection that was never pinned
  is an error naming the `update` line to run. Naming the reference directly is
  equivalent:

  ```sh
  cargo run -p boot2deb-cli -- build turing-rk1/forky+media-accel-rockchip+jellyfin
  ```

  A variant builds in its own work directory under its own image identity, so it never
  lands on the recipe's artifacts.

- **`--stage <node>`** runs a single node — `kernel`, `dtb`, `kmod`, `uboot`,
  `userspace`, `ffmpeg`, `rootfs`, or `image`; the default builds everything. `kmod`
  builds the board's out-of-tree module `.deb`s (its
  [`device_kmods`](config-model.md#out-of-tree-modules-are-their-own-layer))
  against an existing kernel tree, so a
  driver bump need not rebuild the kernel. A `--stage uboot` run
  also emits a standalone, directly-flashable `<point>-boot.img` (see below). Asking for
  a node this recipe does not *have* — `--stage kernel` on a board that installs Debian's
  kernel — is an error naming why, not a silent no-op.
- **`--layout combined|split`** overrides the image packaging. `combined` is one
  whole-disk image; `split` emits a bootloader-only image and a separate rootfs image
  for a two-medium install. This is lock-independent — it changes only how the image is
  packaged, not any pinned source. Only a boot method that *has* a bootloader can split
  it off.
- **`--board <profile>`** selects the depthcharge board profile — which *firmware* the
  signed kernel is built for, not which board. The default is the device's, which is the
  stock profile; `--board speedy-libreboot` targets a C201 running libreboot. Ignored by
  boot methods with no board profile.
- **`--locale`, `--locale-gen`, `--timezone`, `--keymap`** override the localization
  axes: the image's `LANG`, any extra locales compiled into it, the `/etc/localtime`
  zone, and the console keyboard layout. Lock-independent — they change only generated
  rootfs config, not any pinned source. The system locale is *always* generated, so
  `--locale de_DE.UTF-8` needs no matching `--locale-gen`. See
  [Locale, timezone, and keyboard](../localization.md).
- **`--refresh-rootfs`** forces a clean rootfs bootstrap instead of restoring the
  content cache.
- **`--kernel-src`, `--uboot-src`, `--mpp-src`, `--librga-src`, `--libmali-src`,
  `--ffmpeg-base-src`, `--kmod-src`** redirect where a tree is *cloned from*, without
  changing what is built: the commit still comes from the lock, so a local checkout
  holding it makes the fetch near-instant and produces the same result. A board can
  declare several out-of-tree modules, so `--kmod-src` names the one it applies to —
  `--kmod-src aic8800=../aic8800`, repeatable. A name the recipe does not build is an
  error rather than a silently ignored flag.

The rootfs stage is content-cached: the resolved package plan keys a store,
so a rebuild whose *solved* package set is unchanged restores the bootstrapped tree
instead of re-running the multi-minute bootstrap. Because the key is the solved set, a
moved mirror resolves new versions and rebuilds automatically — a cache hit is never
stale. The unique per-image first-boot password is applied on restore, not cached, so
every image still gets its own credential.

### Rebuilding only the board DTB

`build <recipe> --stage dtb` compiles just the board's device tree in the
already-cloned, already-patched kernel tree and stages the `.dtb` — seconds rather than
a full kernel build. It is the bring-up loop for a board carrying its own `device_dts`
source: edit the `.dts`, rebuild the DTB, reflash. The result is byte-identical to the
DTB a full `--stage kernel` ships inside the `linux-image` deb.

### Standalone bootloader image

`build <recipe> --stage uboot` writes `<point>-boot.img` next to the raw
`<point>-idbloader.img` and `<point>-u-boot.itb`, where `<point>` is the build point
with its `/` flattened (`turing-rk1/forky` → `turing-rk1-forky`): a small, GPT-less
image holding just the bootloader at its offsets. It
needs no rootfs, so you can produce a flashable eMMC/SPI bootloader image without building
a whole OS. The `split` layout emits the same image as part of a full build. See
[Turing RK1](../boards/turing-rk1.md) for the eMMC-plus-NVMe workflow this serves.

## Verification

Three read-only commands catch config mistakes before any compile — each exits non-zero
on failure, so they gate CI as well as an interactive bring-up. They share the
reproducibility split: every one reads the recipe's lock for its pins, and any that needs
a source tree **auto-fetches it at the locked commit** into a durable cache, so all three
work on a fresh clone with no hand-cloned trees.

### Which verify when

| What changed / what you want to be sure of | Command |
| --- | --- |
| Imported or edited a patch — does the series still apply to the pinned kernel and u-boot (and ffmpeg/userspace)? | `verify-patches` |
| Edited a `.config` fragment or the base defconfig — does the kernel `.config` still generate cleanly (and match a reference)? | `verify-config` |
| A lock is old — are its pinned commits still fetchable upstream, or has a branch moved out from under them? | `verify-sources` |

The first `verify-patches` or `verify-config` on a cold cache clones the kernel, and
linux-stable is large. If you already have a local checkout, point `--kernel-src` at it
(a git URL or path holding the locked commit) to make the fetch near-instant;
`--ffmpeg-base-src` and `--mpp-src` do the same for the other trees. `verify-sources`
never clones — it only queries the remotes.

#### The free prerequisite: does the series even claim this version?

Before any of those, there is a question that needs no source tree at all — whether
each composed series' declared envelope admits the version being pinned. It is pure
metadata, so `update` and `build` both ask it for free:

- **`update`** says so at pin time and keeps going, because pinning the new version is
  the first step of adopting it. Bumping onto a kernel the series predates is exactly
  the routine move that hits this.
- **`build`** refuses, before cloning anything. The compile nodes ask the same
  question, but only once the tree is on disk — a minute of network for an answer that
  was already in the manifests.

Each axis is asked about its own version: `applies_to_kernel` against the pinned kernel
tag, `applies_to_uboot` against the pinned u-boot tag. A u-boot series makes no claim
about a kernel, so the two never gate each other.

On the kernel axis both name the `verify-patches --kernel` line to run next. That
ordering is the point: the cheap check tells you a series makes no claim about your
kernel, and the expensive one tells you whether it would have worked anyway.

```
note: kernel v7.2-rc5 is outside series 'rk3588-accel' (declared >=7.0, <7.2) — a build
      will refuse it. Measure it first, which needs no re-pin:
  boot2deb verify-patches turing-rk1/forky --kernel v7.2-rc5 --kernel-path <checkout> --keep-going
then widen applies_to_kernel in the series if it comes back clean, or retire the
patches it names.
```

u-boot has no `--kernel` equivalent — there is no "verify against a u-boot the lock does
not pin" mode — so its advisory points straight at the claim:

```
note: u-boot v2027.04 is outside series 'rk3576-display' (declared >=2026.01, <2027.01) —
      a build will refuse it. Verify it first, which needs no re-pin:
  boot2deb verify-patches h96-max-m9/forky --keep-going
then widen applies_to_uboot in the series if it comes back clean, or retire the patches
it names.
```

u-boot's `vYYYY.MM` tags are zero-padded, and both sides of a range accept that
spelling: `applies_to_uboot = ">=2026.01, <2027.01"` matches the tag `v2026.04` as
written.

### verify-patches

```sh
# Dry-run every locked patch series against its source tree with `git am --3way`,
# hard-erroring on the first patch that does not apply. Omit the checkouts and each
# tree is auto-fetched at its pin.
cargo run -p boot2deb-cli -- verify-patches turing-rk1/forky

# Both patch axes are covered. A u-boot-only recipe verifies its u-boot series...
cargo run -p boot2deb-cli -- verify-patches rk3576-generic/loader

# ...and a recipe carrying both reports each at its own version:
#   kernel series applies (4 patches) against rk3576-mainline-7.1 @ v7.1.3
#   uboot  series applies (6 patches) against u-boot @ v2026.04
cargo run -p boot2deb-cli -- verify-patches rk3576-evb1-v10/forky

# Fast path when you already have a local kernel checkout:
cargo run -p boot2deb-cli -- verify-patches turing-rk1/forky --kernel-src ../linux

# "Would this series survive 7.2?" -- asked against a kernel you have not adopted,
# reporting every boundary at once rather than stopping at the first.
cargo run -p boot2deb-cli -- verify-patches turing-rk1/forky \
    --kernel v7.2 --kernel-path ../linux --keep-going
```

#### Asking about a kernel you have not adopted

`--kernel <version>` verifies against a kernel the lock does not pin, and **leaves the
lock alone**. That ordering matters: without it, finding out whether a series survives a
new kernel means re-pinning to that kernel first — mutating state before knowing whether
the answer is yes, which is backwards for the one command whose job is finding out.

Because the lock pins no commit for a kernel it does not name, a candidate needs
`--kernel-path` pointing at a checkout already at that version. Three rules shift on this
path:

- **The declared envelope does not gate the run.** A series is asked about 7.2 exactly
  while its `applies_to_kernel` still says `<7.2`, so refusing an out-of-envelope
  candidate would answer the question by assuming it — the only way past would be to
  widen the claim first, which is the very thing being tested. So the run reports that
  the kernel is outside the envelope and measures it anyway, and what `git am` does is
  the answer. A clean result is the *evidence for* widening the envelope, not a claim
  that it already covers that kernel. On the locked path an out-of-envelope kernel stays
  a hard error: there the series really would be applied to a kernel it makes no claim
  about. Per-entry `kernels` ranges still narrow the series, so a patch already marked
  obsolete at the candidate drops out rather than counting as a failure.
- **A release candidate is answerable.** By semver's rule `7.2.0-rc3` satisfies neither
  `<7.2` nor `>=7.2`, so a release-only range rejects every RC. That strictness is right
  for a build — a series' envelope is a claim about *released* kernels — but wrong
  here, where an RC is exactly the tree you want to measure. On the candidate path an RC
  is matched as its base release; the build path stays release-strict.
- **`--keep-going` reports every failure in one pass.** A single boundary frequently
  spawns adjacent ones: reworking a patch shifts the context every later patch applies
  against. Stopping at the first turns that into serial discovery — fix, re-run, find the
  next, re-run. Each failing patch is skipped so the rest still get measured, which means
  a batch report shows the *shape* of the damage rather than a final verdict; a rework can
  still change what comes after it.

`--kernel-path` / `--uboot-path` / `--ffmpeg-path` / `--userspace-path` are all
**optional**: an omitted tree is auto-fetched at its locked commit (ffmpeg and userspace
only when the series carries patches for that scope). The `--kernel-src` / `--uboot-src`
/ `--ffmpeg-base-src` / `--mpp-src` flags (same names and meaning as `build`'s) override
the fetch *source* — a git URL or local path used in place of the configured upstream —
while the tree still lands at exactly the locked commit; they are consulted only on the
first materialization and ignored when the matching `--*-path` is given. The `patches`
checkout is resolved the way `build` does: an explicit `--patches-path`, else
`../patches` if present, else an auto-fetch at the pinned commit from the repo the
lock's pin names.

`--kernel` is kernel-axis only. A recipe that pins no kernel patch series rejects it
rather than quietly verifying its u-boot series and reporting a green that answers
nothing.

### verify-config

```sh
# Generate the kernel .config (base defconfig + fragments, via merge_config.sh) on the
# patched kernel tree and report the merge. Omit --kernel-path and the tree is fetched
# and the kernel patch series applied for you.
cargo run -p boot2deb-cli -- verify-config turing-rk1/forky

# Assert byte-identical CONFIG_* parity against a reference config as well:
cargo run -p boot2deb-cli -- verify-config turing-rk1/forky --reference-config /path/to/.config
```

`--kernel-path` is optional; omitted, the kernel is auto-fetched at its pin and the kernel
patch series applied before the config run. `--kernel-src` supplies a local fetch source
the same way as `verify-patches`. With `--reference-config`, the run additionally fails on
any `CONFIG_*` difference from the reference.

### verify-sources

```sh
# Survey the durability of every source pin in the lock: for each, probe its configured
# upstream and report whether the commit is a durable tag, an ephemeral branch, or
# ORPHANED (no longer re-fetchable). Read-only: `git ls-remote` plus a bounded ancestry
# check -- no build, no checkout, no hardware.
cargo run -p boot2deb-cli -- verify-sources turing-rk1/forky
```

`verify-sources` answers "will this lock still build a year from now?" An orphaned pin
(a branch force-pushed, a tag deleted upstream) exits non-zero, so a periodic run catches
a lock rotting before a build needs it. Capture a snapshot (`build --save-snapshot`) to
make the rootfs solve durable the same way.

### patch import

`patch import` fetches a patch, normalizes it to canonical `git am`-ready mbox, and slots
it into a series — the first step of the patch-authoring loop. It is documented with its
full workflow (commit, re-pin, verify) on
[Adding a patch](../contributing/adding-a-patch.md):

```sh
cargo run -p boot2deb-cli -- patch import https://patchwork.kernel.org/project/linux-rockchip/patch/NNNN/mbox/ \
  --series rk3588-accel --scope kernel
```

## Rebuild planning and cleanup

```sh
# Explain, offline, whether the next build reuses or rebuilds each compile node's source
# tree -- and which pinned input changed if it will rebuild.
cargo run -p boot2deb-cli -- why-rebuild turing-rk1/forky

# Remove a recipe's build scratch to reclaim disk or force a clean rebuild. --dry-run
# previews; --cache / --sandbox clean only that subtree.
cargo run -p boot2deb-cli -- clean turing-rk1/forky --dry-run
```

`clean` removes only directories `build` created: every work dir is stamped with a
`.boot2deb-work` marker, and an unmarked target is refused — so a mistyped
`--work-dir` cannot recursively delete an arbitrary tree. `--force` overrides the
check for a directory you are sure about.

`why-rebuild` reads the lock and each compile node's signature stamp and reports, per node,
whether the next `build` reuses or rebuilds the cloned-and-patched tree, naming the pinned
input that moved when it will rebuild. It runs no build and touches no network.
