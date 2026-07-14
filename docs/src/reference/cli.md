# CLI

The binary is `boot2deb`; during development run it with `cargo run -p boot2deb-cli --`.
It defaults `--root .`, so run it from inside `boot2deb/` (or pass `--root`).

Three global flags apply to every command: `--root <dir>` (the config root),
`--overlay <dir>` (an out-of-tree config overlay, repeatable — see
[Overlays](overlays.md)), and `--json` (machine-readable output).

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
cargo run -p boot2deb-cli -- resolve turing-rk1-forky
cargo run -p boot2deb-cli -- resolve turing-rk1 --suite sid --layout split
cargo run -p boot2deb-cli -- doctor turing-rk1-forky
```

- **`list-devices` / `list-recipes`** enumerate the buildable targets; `list-recipes`
  flags any recipe with no committed lock as not-yet-buildable (run `update`).
- **`list-kernels` / `list-features`** enumerate the valid values for the `--kernel`
  and `--feature` overrides — name, version/compatibility, and (for kernels) the patch
  profile — so the override knobs are discoverable without reading the TOML tree.
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
# choices, then writes devices/<name>.toml + recipes/<name>.toml.
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

- **`--stage <node>`** runs a single node — `kernel`, `dtb`, `uboot`, `userspace`,
  `ffmpeg`, `rootfs`, or `image`; the default builds everything. A `--stage uboot` run
  also emits a standalone, directly-flashable `<device>-boot.img` (see below). Asking for
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

The rootfs stage is content-cached: a cheap `mmdebstrap --simulate` solve keys a store,
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

`build <recipe> --stage uboot` writes `<device>-boot.img` next to the raw `idbloader.img`
and `u-boot.itb`: a small, GPT-less image holding just the bootloader at its offsets. It
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
| Imported or edited a patch — does the series still apply to the pinned kernel (and ffmpeg/userspace)? | `verify-patches` |
| Edited a `.config` fragment or the base defconfig — does the kernel `.config` still generate cleanly (and match a reference)? | `verify-config` |
| A lock is old — are its pinned commits still fetchable upstream, or has a branch moved out from under them? | `verify-sources` |

The first `verify-patches` or `verify-config` on a cold cache clones the kernel, and
linux-stable is large. If you already have a local checkout, point `--kernel-src` at it
(a git URL or path holding the locked commit) to make the fetch near-instant;
`--ffmpeg-base-src` and `--mpp-src` do the same for the other trees. `verify-sources`
never clones — it only queries the remotes.

### verify-patches

```sh
# Dry-run every locked patch series against its source tree with `git am --3way`,
# hard-erroring on the first patch that does not apply. Omit the checkouts and each
# tree is auto-fetched at its pin.
cargo run -p boot2deb-cli -- verify-patches turing-rk1-forky

# Fast path when you already have a local kernel checkout:
cargo run -p boot2deb-cli -- verify-patches turing-rk1-forky --kernel-src ../linux
```

`--kernel-path` / `--ffmpeg-path` / `--userspace-path` are all **optional**: an omitted
tree is auto-fetched at its locked commit (ffmpeg and userspace only when the profile
carries patches for that scope). The `--kernel-src` / `--ffmpeg-base-src` / `--mpp-src`
flags (same names and meaning as `build`'s) override the fetch *source* — a git URL or
local path used in place of the configured upstream — while the tree still lands at
exactly the locked commit; they are consulted only on the first materialization and
ignored when the matching `--*-path` is given. The `patches` checkout is resolved the way
`build` does: an explicit `--patches-path`, else `../patches` if present, else an
auto-fetch at the lock's `patches.commit`.

### verify-config

```sh
# Generate the kernel .config (base defconfig + fragments, via merge_config.sh) on the
# patched kernel tree and report the merge. Omit --kernel-path and the tree is fetched
# and the kernel patch series applied for you.
cargo run -p boot2deb-cli -- verify-config turing-rk1-forky

# Assert byte-identical CONFIG_* parity against a reference config as well:
cargo run -p boot2deb-cli -- verify-config turing-rk1-forky --reference-config /path/to/.config
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
cargo run -p boot2deb-cli -- verify-sources turing-rk1-forky
```

`verify-sources` answers "will this lock still build a year from now?" An orphaned pin
(a branch force-pushed, a tag deleted upstream) exits non-zero, so a periodic run catches
a lock rotting before a build needs it. Capture a snapshot (`build --save-snapshot`) to
make the rootfs solve durable the same way.

### patch import

`patch import` fetches a patch, normalizes it to canonical `git am`-ready mbox, and slots
it into a profile — the first step of the patch-authoring loop. It is documented with its
full workflow (commit, re-pin, verify) on
[Adding a patch](../contributing/adding-a-patch.md):

```sh
cargo run -p boot2deb-cli -- patch import https://patchwork.kernel.org/project/linux-rockchip/patch/NNNN/mbox/ \
  --profile rk3588-accel --scope kernel
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

`clean` removes only directories `build` created: every work dir is stamped with a
`.boot2deb-work` marker, and an unmarked target is refused — so a mistyped
`--work-dir` cannot recursively delete an arbitrary tree. `--force` overrides the
check for a directory you are sure about.

`why-rebuild` reads the lock and each compile node's signature stamp and reports, per node,
whether the next `build` reuses or rebuilds the cloned-and-patched tree, naming the pinned
input that moved when it will rebuild. It runs no build and touches no network.
