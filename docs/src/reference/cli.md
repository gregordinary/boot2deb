# CLI

This page explains what the commands are *for*. For the exhaustive list of every
flag on every command — generated from the binary, so it cannot drift — see
[Every flag](cli-flags.md), or run `boot2deb <command> --help`.

The binary is `boot2deb`, installed with `cargo install --path crates/cli` (see
[Getting started](../getting-started.md)); working from a checkout without installing,
prefix each command with `cargo run -p boot2deb-cli --`. It defaults `--root .`, so run
it from inside `boot2deb/` (or pass `--root`).

Five global flags apply to every command: `--root <dir>` (the config root),
`--overlay <dir>` (an out-of-tree config overlay, repeatable — see
[Overlays](overlays.md)), `--json` (machine-readable output), and `--quiet`/`--verbose`.

`--root` moves everything, not just where config is read from. A run's durable
state is anchored to the config root: the build scratch (`<root>/build/<recipe>`),
the artifact, patches, extra-deb, and verify-tree caches (`<root>/cache/...`), and
the default `patches` checkout (the root's sibling `../patches`). So `--root
boot2deb why-rebuild turing-rk1/forky` from the parent directory inspects the same
trees a run from inside `boot2deb/` builds into. An explicit `--work-dir` or
`--patches-path` is taken as given, relative to the current directory.

## How much a build prints

A build's event stream carries two very different volumes: what each stage *decided*
(tens of lines) and what its subprocesses *printed* (tens of thousands). The default
shows the first, so a tens-of-minutes kernel compile stays readable:

| level | what you see |
| --- | --- |
| `--quiet` | artifact paths and errors only — what the command produced, nothing about getting there |
| *(default)* | step boundaries, coarse progress, each stage's own decisions, artifacts, errors |
| `--verbose` | the above plus every line `make`, `git`, and `dpkg-buildpackage` emit |

Reach for `--verbose` when a stage fails or hangs: it is the level that shows what the
failing subprocess actually said. When what it said is not enough, [`shell`](#shell)
puts you inside the root it said it in.

At the default level a build ends with where its time went:

```
timing:
kernel     12m04s  built
uboot      3.0s    restored
userspace  1m00s   partly restored
rootfs     4m31s   built
image      1m12s   built
total      18m02s
```

The second column is what makes the first readable — a three-second kernel step is
the artifact cache answering, not a fast compiler. `restored` means every one of that
step's outputs came back from the cache and nothing was compiled; `partly restored`
is a step whose outputs cache one at a time (the userspace stage builds several
`.deb`s, each with its own signature) where some were restored and some were built.

`total` is the command's own wall clock, so it exceeds the sum of the rows by
whatever the build does outside any step. The summary is suppressed under `--quiet`
and under `--json`, where the same numbers ride on each `step_finished` event.

A build that wrote an image closes with what to do with it:

```
next: write the image to /dev/sdX — confirm the device with `lsblk` first, since dd
      overwrites it whole
      xzcat build/turing-rk1/forky/artifacts/turing-rk1-forky.img.xz | sudo dd \
        of=/dev/sdX bs=4M status=progress conv=fsync
```

The paths are the files the run actually produced, so a `--compress none` build hints
the raw `.img` and a `split` build hints both halves with the medium each goes to.
The pipe matches the container: `xzcat` for a `.xz` and `zcat` for a `.gz`, and where a
build asked for both (`--compress xz,gz`) the hint names the one asked for first.
`/dev/sdX` is a placeholder in every case — a build cannot know which disk is meant,
and a real device node in a copy-pasteable `dd` line is how the wrong disk gets
overwritten. Boards with a flashing route of their own (the RK1's `tpi`, a Chromebook's
recovery media) document it on their [board page](../boards/turing-rk1.md).

## Machine-readable output

`--json` gives a machine form to the commands a script consumes:

| command | `--json` form |
| --- | --- |
| `list-*` | one JSON array; an unreadable entry rides along as `{"name", "error"}` |
| `resolve` | the fully resolved build as one JSON document. Everything only an *image* has — the kernel, suite, rootfs set, localization, account, out-of-tree modules, media-accel sources — is nested under `image`, which is absent on a `deliverable = "uboot"` recipe |
| `doctor` | host facts, every check with its status and remedy, the trust anchors, and a `result` |
| `verify-patches` | per axis: how many patches applied, and every one that did not |
| `verify-config` | the merge or parity verdict, with each differing `CONFIG_*` |
| `verify-sources` | per pin: its durability class and the detail behind it |
| `verify-packages` | the present / provided / missing split, plus the names this build produces itself |
| `verify-image` | every checked image invariant with its detail and verdict, plus a `result` |
| `diff` | every section's comparison as one document, with the per-series patch-file deltas under `patch_files` |
| `outdated` | per recipe, every pin with its verdict flattened in — `status` plus the newer release or moved tip behind it |
| `size` | the whole rollup: every row with its weight and package count, plus the totals. Untruncated, whatever `--top` says |
| `build` | NDJSON — one object per line, tagged by its `event` field (`step_started`, `progress`, `log`, `artifact`, `step_finished`, `error`), with every produced artifact's path on an `artifact` event and each step's `duration_ms` + `outcome` (`built`/`restored`/`mixed`) on its `step_finished` |

Errors are still plain text on stderr, and the exit code is the result either way.
`--quiet`/`--verbose` do not apply under `--json`: the stream *is* the record of the
build, and a filtered record would be a wrong one.

A command with no machine form — `update`, `clean`, `why-rebuild`, `new-device`,
`support-matrix`, `patch import`, `sbom` — **rejects** `--json` rather than ignoring it,
naming the structured route to the same information where one exists. A global flag that
silently did nothing would be a trap for exactly the scripted caller it exists for.

The two commands that split reproducibility from upstream are `update` (the only one
that consults the network) and `build` (reads only the lock). See
[Config model](config-model.md) for that split.

## Inspection

```sh
boot2deb list-devices
boot2deb list-recipes
boot2deb list-kernels
boot2deb list-features
boot2deb list-kmods
boot2deb support-matrix
boot2deb resolve turing-rk1/forky
boot2deb resolve turing-rk1 --suite trixie --layout split
boot2deb doctor turing-rk1/forky
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
  existence, feature compatibility, apt keyrings). Every selectable axis (`--kernel`,
  `--suite`, `--feature`, `--layout`, `--boot-method`, `--board`, `--image-size`,
  `--locale`, `--locale-gen`, `--timezone`, `--keymap`) can be overridden, so you can
  see what a choice resolves to before committing it to config.

  It accepts a wider set than any command that can *build* the result, and says so when
  that matters: an override `build` does not take closes the printout with the recipe
  file to write, ready to paste.

  ```
  note: --suite is resolve-only — `build` reads that axis from the config its lock was
  resolved against, not from a flag. To build this point, write it down:
  recipes/turing-rk1/<leaf>.toml with
      device = "turing-rk1"
      suite  = "trixie"
  then `boot2deb update turing-rk1/<leaf>` to pin it.
  ```

  `--boot-method` is the one axis a recipe cannot express — how a board boots is a
  property of the hardware — so its note names a device file instead. See
  [Adapting a shipped recipe](../tutorials/adapting-a-recipe.md).
- **`doctor`** reports the host's tool-presence preflight and, for anything missing, the
  exact per-distro install command. With a target it asks only for what *that build*
  will invoke: a board that installs Debian's kernel and boots its own firmware compiles
  nothing, so it is not told to install a cross compiler — which keeps a genuinely
  missing tool from getting lost among requirements that do not apply. Bare, it runs the
  requirements every board shares (user namespaces, the `.deb` packaging tools, the
  vendored apt trust anchors), so it is useful before a recipe is chosen. Either way a
  missing required tool is a non-zero exit, so it gates CI. See
  [Getting started](../getting-started.md).
- **`cli-reference`** prints [Every flag](cli-flags.md) — the whole argument surface,
  generated from the command tree. `--markdown` regenerates the committed page, and a
  test fails when it goes stale.
- **`completions <shell>`** and **`man`** print a shell completion script and the
  `boot2deb(1)` man page on stdout, generated from the same command tree. They install
  nothing: where those files belong is the packager's call.

  ```sh
  boot2deb completions bash > ~/.local/share/bash-completion/completions/boot2deb
  boot2deb man > ~/.local/share/man/man1/boot2deb.1
  ```

## Scaffolding

```sh
# Interactive on a terminal: menus over the valid SoC / boot-method / kernel / feature
# choices, then writes devices/<name>.toml + recipes/<name>/<suite>.toml.
boot2deb new-device my-board

# Scriptable: take every value from flags (required: --soc), no prompts.
boot2deb new-device my-board --soc rk3588 \
  --feature media-accel-rockchip --non-interactive

# Scaffold into your own overlay tree instead of the shipped root:
boot2deb --overlay ~/my-boards new-device my-board --soc rk3588
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
boot2deb update turing-rk1/forky --kernel-ref v7.1.1
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
boot2deb update turing-rk1/forky --feature media-accel-rockchip --feature jellyfin
# wrote recipes/turing-rk1/forky+media-accel-rockchip+jellyfin.lock
```

## build

```sh
boot2deb build turing-rk1/forky
```

Builds the recipe from its lock: compiles the kernel, u-boot, userspace, and ffmpeg,
bootstraps the rootfs, and writes the bootable disk image. Notable flags:

- **`--feature <name>`**, repeatable, selects which lock to build — the one `update
  --feature` pinned. It does not re-resolve one, so a selection that was never pinned
  is an error naming the `update` line to run. Naming the reference directly is
  equivalent:

  ```sh
  boot2deb build turing-rk1/forky+media-accel-rockchip+jellyfin
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
- **`--image-size <size>`** overrides the image size the same way. The rootfs grows to
  fill its medium on first boot, so this bounds the *artifact*, not the installed system.
  It also takes the measured form — `--image-size fit+20%` builds the smallest image that
  holds the rootfs with a fifth of it free, which is the quickest way to find out how
  large a new board's image actually needs to be. See
  [An image size can be stated or measured](config-model.md#an-image-size-can-be-stated-or-measured).
- **`--refresh-rootfs`** forces a clean rootfs bootstrap instead of restoring the
  content cache; **`--no-artifact-cache`** forces every compile node to rebuild instead
  of restoring stored `.deb`s (see [Two caches](#two-caches-and-what-each-one-keys-on)).

- **`--kernel-src`, `--uboot-src`, `--ffmpeg-base-src`, `--userspace-src`,
  `--kmod-src`** redirect where a tree is *cloned from*, without
  changing what is built: the commit still comes from the lock, so a local checkout
  holding it makes the fetch near-instant and produces the same result. A SoC declares
  several userspace trees and a board several out-of-tree modules, so those two name the
  one they apply to — `--userspace-src mpp=../mpp-rockchip`, `--kmod-src
  aic8800=../aic8800`, both repeatable. A name the recipe does not build is an
  error rather than a silently ignored flag.

`build` takes no `--kernel`, `--suite`, `--board`, `--locale`, `--timezone`, or
`--keymap`. Those axes come from the config the recipe's lock was resolved against, not
from a flag: `resolve` accepts them so you can see what a choice resolves to, and then
says so — naming the recipe file to write if you want to build it. See
[Adapting a shipped recipe](../tutorials/adapting-a-recipe.md) for that path, and
[Locale, timezone, and keyboard](../localization.md) for the localization axes in
particular.

### Two caches, and what each one keys on

Nothing about a rebuild is obvious from the outside, so it is worth knowing which of
the two caches answers which question. `why-rebuild` reports both, per node.

**The rootfs cache** keys on the *solved package set*. A rebuild whose solve is
unchanged restores the bootstrapped tree instead of re-running the multi-minute
bootstrap. Because the key is the solved set and not the requested one, a moved mirror
resolves new versions and rebuilds automatically — a hit is never stale. The unique
per-image first-boot password is applied on restore rather than cached, so every image
still gets its own credential. The *rest* of the account policy — the sudo drop-in and
the authorized keys — is part of the tree and part of the key, so authorizing a key or
tightening `sudo` rebuilds rather than restoring a tree with the old rules.
`--refresh-rootfs` forces a clean bootstrap.

**The artifact cache** keys on each compile node's *full set of output-determining
inputs* — the source pins and patch series, the kconfig fragments' contents, the
defconfig, the identity of the root the stage compiled in, and the build-dependencies it
layered over that root. On a hit, `build` restores that node's stored
`.deb`s and **skips the compile entirely**: the single largest lever there is, since it
is the difference between restoring a file and a 30-minute kernel cross-compile or a
70-minute emulated ffmpeg build.

It lives at `<root>/cache/artifacts`, outside any recipe's work dir — so it survives
`clean`, and is shared across work dirs and recipes. A freshly cloned checkout with no
build tree at all can still restore every `.deb` and compile nothing. `clean --artifacts`
empties it (for *every* recipe, since the store is shared); `--no-artifact-cache` on a
build ignores it and stores nothing.

Because the key covers every input that can change the output, a hit is sound: two
builds that would produce different `.deb`s cannot share an entry.

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

## try

```sh
# Boot the built image twice under QEMU and assert the userland works —
# multi-user with no failed units, the generated password logs in, first-boot
# completes and does not re-run, and the on-image selftest passes.
boot2deb try turing-rk1/forky
```

The step between `build` and `press`: it catches the image that flashes fine
and is quietly broken — a userland fault, a brick-on-second-boot — while the
fix is still a rebuild rather than a reflash-and-serial-console session. The
board kernel is not booted (the guest runs the suite's generic kernel as a
fixture) and no board hardware exists under `-M virt`, so this tests the
userland and only the userland. [Trying an image before flashing](try.md) has
the full contract, including what the two boots each assert and the fixture
kernel's mechanics.

## press

```sh
# Produce the distributable image file, verified.
boot2deb press turing-rk1/forky card.img

# The same, personalized per unit and previewed first.
boot2deb press turing-rk1/forky rk1-03.img --hostname rk1-03 \
    --ssh-key "$(cat ~/.ssh/id_ed25519.pub)" --dry-run

# Per-site additions re-assemble the image from the kept artifacts.
boot2deb press asus-c201/forky card.img --embed-image \
    --copy site.conf:/etc/myapp/site.conf
```

What a press produces is derived from the resolved build, not from flags: a
`combined` build is one file, a u-boot deliverable is its boot image, and a
`split` build refuses a single positional output and names `--boot-out` +
`--rootfs-out`. A plain press streams the existing artifact and verifies the
file it wrote (digest re-read + partition-table compare); a press with
`--copy`/`--deb`/`--embed-image` re-assembles the image from the kept rootfs
tar. boot2deb does not write devices — hand the file to `dd` or a real flasher.
[Producing images](../press.md) is the full story, including the seed keys and
the pressed-image provenance marker.

## seed

```sh
# Re-personalize an already-pressed image file without re-pressing it.
boot2deb seed rk1-03.img --hostname rk1-04 --wifi-ssid lab --wifi-psk '...'
```

No recipe: the seed partition is found by its GPT label, so the file is the
whole input. With no keys the seed resets to the empty template. Files only —
a card that is already written is re-personalized by editing `seed.txt` on its
`B2D-SEED` volume directly.

## shell

```sh
# A shell in the root the kernel compiles in.
boot2deb shell turing-rk1/forky --stage kernel

# Or one command in it, non-interactively.
boot2deb shell turing-rk1/forky --stage kernel -- make olddefconfig
```

When a compile fails, `--verbose` shows you what it printed. `shell` is the other way
in: it stands the stage's root up and hands you a prompt inside it, with the same base
tree, the same layered build-dependencies, the same mounts, the same environment and the
same identity map the compile had. You start in the stage's own tree — `make` re-runs
verbatim, `ARCH` and `CROSS_COMPILE` are already set for the kbuild stages, and you are
`root`, as every command in these roots is.

`--stage` names the root, and is required — the point is entering a *particular* one:

| `--stage` | the root | layered with |
| --- | --- | --- |
| `kernel` | the host-arch cross root | the kernel stage's build-deps |
| `uboot` | the same cross root | the u-boot stage's build-deps |
| `kmod` | the same cross root | the kernel's, which is what an out-of-tree module build needs |
| `userspace` | the target-arch build sandbox | the userspace stage's shared build-deps, plus each named tree's own (`--userspace <name>`) |
| `ffmpeg` | the same target-arch sandbox | the suite's codec libraries **plus this build's own `librga`/MPP `.deb`s**, so run `--stage userspace` first |
| `packaging` | the host-arch packaging root | nothing — it is never layered |

The work dir is bound at its host path, so every stage's tree, scratch and output is
there and edits you make inside are on the host when you leave — as are the config
root's kernel fragments and board device trees, which the kernel stage binds the same
way. Everything else you write goes into the session's own overlay and is gone when you
exit. The root has **no network**, exactly as a compile does not: everything a build root
needs is resolved before it is entered.

Two things to know about what you are entering. The layer is **re-staged, not
reattached**: a build root is discarded when its stage ends, so what you get is the root
that stage's declaration produces and not the failed run's writable layer — what the
compile wrote into the *work dir* is still there, what it wrote into `/usr` is not. And
the session's layer is staged under its own name, so opening a shell while a build of
the same recipe is running does not disturb it.

The session's exit status is `boot2deb`'s own, so `shell <recipe> --stage kernel -- make
foo` in a script reports what `make` reported. It needs a terminal: `shell` relays yours
to a pseudoterminal inside the sandbox, and refuses rather than starting a session with
nothing on one end. `tty`, `who`, and `GPG_TTY` have no answer inside — the terminal is
allocated on the host, so it has no device node in the sandbox — while everything else a
terminal does, including full-screen programs, job control, and running `tmux`, works.

If the root has never been provisioned in this work dir, the first `shell` bootstraps it,
which is the same minutes a first build would spend. Later ones reuse the tree.

## reproduce

```sh
boot2deb reproduce turing-rk1/forky --from ./published
```

Rebuilds an image from the **plan document** a previous build published, rather than
resolving the archive afresh. It takes every `build` flag and runs the same pipeline;
what differs is the rootfs, which installs the plan's exact package set by the digests
the plan records — reading neither a `Release` nor a package index.

The lock pins sources, patches, and the builder. It cannot pin *which package versions
the archive served*, so the same lock a month later resolves a different userland. The
plan pins exactly that, and it is written beside every image as `<point>.plan`:

```
turing-rk1-forky.img.xz
turing-rk1-forky.provenance.toml
turing-rk1-forky.plan          <- the document reproduce replays
turing-rk1-forky.pkgs.lock
```

It is deb822 — the archive's own control format — so it reviews as a diff. Each stanza
names a package's version, architecture, sha256, pool path, and which archive it came
from; a leading stanza per archive records the mirror that answered, the suite and
components, the sha256 of the release body that was verified, its `Date` and
`Valid-Until`, and the fingerprint of the key that verified it.

`--from` names the directory holding that document; it defaults to this build point's
own output directory, so re-running a build to check that it *is* reproducible needs no
flag. The provenance manifest beside it is read for one advisory line — which boot2deb
produced the image, and how the running checkout compares. That is advice and never a
gate: a stamped commit is the commit at which the build worked, never the commit past
which it breaks.

**This moves the trust anchor, deliberately.** An ordinary build's package digests come
from an index whose own digest a signed release vouched for, so they chain to the archive
signature. A replay never reads that index, so the digests chain to the plan document
instead. Each `.deb` is still verified against the digest the plan records — a mirror
serving different bytes is caught — but nothing re-checks that the plan describes a set
the archive ever offered. That trade is right for reproducing a published image and wrong
for a routine build, which is why it is reachable only through this command; `build` has
no flag for it.

**A recipe that compiles its own packages replays only if those compiles are
byte-reproducible.** The plan pins the sha256 of the kernel `.deb` — and, on a
media-accel recipe, of `ffmpeg-rk`, `librockchip-mpp1` and `librga2` — that the original
build produced, because those install from the build's own local pool like any other
package. Replay them and either the digests match, which proves the whole image
reproduced, or the install fails naming the package that drifted. The second outcome is
the honest one: it says this recipe is not yet reproducible, rather than quietly
producing a different image. A recipe that installs Debian's own kernel and compiles
nothing has no such dependency.

Pair it with a snapshot pin for the strongest form. The plan says which versions; the
lock's `snapshot.debian.org` timestamp keeps those versions *fetchable* after they rotate
off the live mirror. See [Reproducibility](reproducibility.md).

## Verification

Four read-only commands catch config mistakes before any compile — each exits non-zero
on failure, so they gate CI as well as an interactive bring-up. They share the
reproducibility split: every one reads the recipe's lock for its pins, and any that needs
a source tree **auto-fetches it at the locked commit** into a durable cache, so all four
work on a fresh clone with no hand-cloned trees.

### Which verify when

| What changed / what you want to be sure of | Command |
| --- | --- |
| Imported or edited a patch — does the series still apply to the pinned kernel and u-boot (and ffmpeg/userspace)? | `verify-patches` |
| Edited a `.config` fragment or the base defconfig — does the kernel `.config` still generate cleanly (and match a reference)? | `verify-config` |
| A lock is old — are its pinned commits still fetchable upstream, or has a branch moved out from under them? | `verify-sources` |
| Added a package to a layer, a feature, or a recipe — does the suite you build against actually carry it? | `verify-packages` |
| A build finished — is the image it produced internally consistent, before you flash it? | `verify-image` |

The first `verify-patches` or `verify-config` on a cold cache clones the kernel, and
linux-stable is large. If you already have a local checkout, point `--kernel-src` at it
(a git URL or path holding the locked commit) to make the fetch near-instant;
`--ffmpeg-base-src` and `--userspace-src` do the same for the other trees. `verify-sources`
never clones — it only queries the remotes.

`verify-packages` clones nothing either. It runs the read half of a package resolve —
the archive's `Release` and its package indexes, and then stops — against the same
archives a build would use: the mirror the lock's snapshot pins (or the live one), plus
every repository the selected features contribute. One pass answers every name at once,
which is why it is cheap enough to run per board over every recipe.

It is worth having as its own command because the resolver cannot answer it. A recipe
naming a package the suite does not carry fails at resolve time — deep in a build, after
every compile node has already run — and fails badly: a top-level include naming nothing
makes the *whole* set unsatisfiable, so the error says the set could not be resolved and
never which names were the problem.

Two kinds of name are reported rather than failed. A package the build **produces** —
anything a `requires_media_accel` feature contributes, which comes from the SoC's source
trees through the build's own local pool — is set aside, since the archives are rightly
silent about it. And a name something else `Provides` is listed with its providers,
because apt then has a choice the recipe did not make.

Once every name is accounted for, it asks the second question: does the set **close**?
A package being in the archive says nothing about its dependencies being there, and the
difference matters more than it sounds. A package whose dependency is absent still
installs — dpkg configures with `--force-depends` — so the build succeeds, the image
flashes, and what breaks is `apt` on the running board, for every package rather than
the one at fault. Resolving the closure here is what turns that into a line of output
before anything compiles:

```text
UNSATISFIED: jellyfin-server requires libicu76, and no configured archive offers it
             and no base layer supplies it
```

Every refusal is reported, not just the first, because the list is what a user has to
correct. The closure runs only when the name check passed: a name the archive does not
carry refuses its own dependency group as well, and reporting that twice would bury
whatever else was found.

The same blind spot applies here as above, and the check accounts for it rather than
crying wolf. A dependency satisfied by a package this build produces, or by one of the
recipe's pre-built `[[extra_debs]]`, cannot be seen by a resolve — neither is in an
archive, and the local pool does not exist until a build runs. Such a refusal is reported
as a note and does not fail the recipe:

```text
local : jellyfin-server requires libicu76 — supplied by this build, not by an archive
```

An `extra_debs` name comes from its filename (`<package>_<version>_<arch>.deb`), because
reading it out of the file would mean downloading and unpacking every pin — which is the
one thing this command promises not to do. A filename that does not follow the convention
explains nothing, and the refusal it would have covered is reported: the safe direction
for a heuristic. Only the *name* is matched, never a version constraint — a local `.deb`
is pinned by digest and this cannot know what version is inside it, so the constraint
stays the build's problem.

A resolution stopped that way has no closure size, and the output says that instead of
printing zero. Under `--json`, `closure.installed` is `null` in that case, `refusals`
carries what the recipe must correct, and `supplied_locally` carries what the check could
not see.

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
boot2deb verify-patches turing-rk1/forky

# Both patch axes are covered. A u-boot-only recipe verifies its u-boot series...
boot2deb verify-patches rk3576-generic/loader

# ...and a recipe carrying both reports each at its own version:
#   kernel series applies (4 patches) against rk3576-mainline-7.2 @ v7.2
#   uboot  series applies (6 patches) against u-boot @ v2026.04
boot2deb verify-patches rk3576-evb1-v10/forky

# Fast path when you already have a local kernel checkout:
boot2deb verify-patches turing-rk1/forky --kernel-src ../linux

# "Would this series survive 7.2?" -- asked against a kernel you have not adopted,
# reporting every boundary at once rather than stopping at the first.
boot2deb verify-patches turing-rk1/forky \
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
/ `--ffmpeg-base-src` / `--userspace-src` flags (the first three the same names and
meaning as `build`'s; the last names the patched tree's source) override
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
boot2deb verify-config turing-rk1/forky

# Assert byte-identical CONFIG_* parity against a reference config as well:
boot2deb verify-config turing-rk1/forky --reference-config /path/to/.config
```

`--kernel-path` is optional; omitted, the kernel is auto-fetched at its pin and the kernel
patch series applied before the config run. `--kernel-src` supplies a local fetch source
the same way as `verify-patches`. With `--reference-config`, the run additionally fails on
any `CONFIG_*` difference from the reference.

### verify-image

```sh
# Hold a finished image to the invariants that are checkable without a board.
boot2deb verify-image turing-rk1/forky
boot2deb verify-image turing-rk1/forky --out-dir /path/to/artifacts
```

The off-board half of the hardware gate, and the last thing worth running before a flash.
Per image it checks that the artifact set is present, that the plan document parses and
its digest matches what the provenance manifest records, that `[[archives]]` is
well formed (the mirror plus the build's own pool, the pool marked `local` and carrying no
mirror URL, since a per-run path is not portable provenance), that **the ext4 filesystem
is exactly its GPT partition**, and — for a fitted `--image-size` — that the slack the
recipe asked for actually survived into the shipped filesystem.

The filesystem/partition check is the one that matters most: larger and it will not mount
at all, smaller and the difference is wasted. It is checked on every image, not only the
fitted one, because it is the invariant the fit ordering exists to preserve.

Every structure is read by the code that *wrote* it — the same Rust GPT and ext4 readers
the image node uses — so the check cannot drift from the build by parsing the same bytes
differently. Read-only and no root: only the head of the artifact is decompressed, so a
compressed multi-gigabyte image costs a few hundred kilobytes. A failing invariant exits
non-zero, and `--json` gives the whole run as one document.

### verify-sources

```sh
# Survey the durability of every source pin in the lock: for each, probe its configured
# upstream and report whether the commit is a durable tag, an ephemeral branch, or
# ORPHANED (no longer re-fetchable). Read-only: `git ls-remote` plus a bounded ancestry
# check -- no build, no checkout, no hardware.
boot2deb verify-sources turing-rk1/forky
```

`verify-sources` answers "will this lock still build a year from now?" An orphaned pin
(a branch force-pushed, a tag deleted upstream) exits non-zero, so a periodic run catches
a lock rotting before a build needs it. Capture a snapshot (`build --save-snapshot`) to
make the rootfs solve durable the same way.

It reads the same ref advertisement as [`outdated`](#what-has-moved-upstream) and
answers the other half of the question: this one is about whether a pin can still be
*fetched*, that one about whether something *newer* exists. Neither implies the other.

### patch import

`patch import` fetches a patch, normalizes it to canonical `git am`-ready mbox, and slots
it into a series — the first step of the patch-authoring loop. It is documented with its
full workflow (commit, re-pin, verify) on
[Adding a patch](../contributing/adding-a-patch.md):

```sh
boot2deb patch import https://patchwork.kernel.org/project/linux-rockchip/patch/NNNN/mbox/ \
  --series rk3588-accel --scope kernel
```

## Comparing two build points

```sh
# Two recipes.
boot2deb diff turing-rk1/forky turing-rk1/media-accel-forky

# One recipe against an older copy of its own lock — git supplies the older one.
git show HEAD~5:recipes/turing-rk1/media-accel-forky.lock > /tmp/old.lock
boot2deb diff /tmp/old.lock turing-rk1/media-accel-forky

# Two shipped images, from the provenance manifests beside them.
boot2deb diff a/turing-rk1-forky.provenance.toml b/turing-rk1-forky.provenance.toml
```

Each side is a **recipe name**, a path to a **`.lock`**, or a path to a
**`.provenance.toml`**, and the two sides need not be the same kind. Everything it
reads is a document the build already wrote, so it runs offline and builds nothing.

Six sections, in the order they answer the question:

| section | what it compares |
| --- | --- |
| `packages` | the solved manifest: added, removed, re-versioned, and *rebuilt* (same version, different `.deb`) |
| `kernel` | the pin — id, flavor, clone URL, ref, commit — and the requested kconfig, symbol by symbol |
| `patches` | series membership, each axis's ref and commit, and the patch **files** behind a moved commit |
| `sources` | every other pinned tree: u-boot, MPP, RGA, Mali, ffmpeg, each out-of-tree module |
| `blobs` | the rkbin pins, by sha256 |
| `builder` | which boot2deb ran, the host it cross-compiled from, and the archive state the rootfs resolved against |

Narrow it with `--section` (repeatable); `--json` gives the whole report as one
document, with the patch-file deltas under `patch_files`.

**Unavailable is not unchanged.** A section neither side records says so rather than
reporting agreement:

```
builder: not compared — neither side records a provenance manifest, which is where
the builder and archive state are recorded
```

Which side is silent is named when only one is, so you know whether to go find the
other document or accept that it does not exist.

Two sections answer more when you name a **recipe** than when you name a document.
The kconfig delta is one: a fragment set is resolved from the config tree, and no
document a build writes names it — so `diff` reads the fragments a recipe's kernel
merges and reports each differing symbol *with the fragment that set it*, which
diffing two generated `.config` files cannot do. A distro-package kernel merges no
fragments at all, and that section reports itself unavailable rather than claiming
every symbol the other side enables is new.

The patch-file delta is the other reach outside those documents: it resolves a moved
patches-repo commit into named files by reading the `patches` repo.

```
patches:
  kernel:
    commit  adfdc19d7caf -> 659033b7e543
    rk3588-accel:
      +  rocket/088-rocket-drv-reset-before-iommu-detach.patch
      ~  media-accel/kernel/060-vepu580-rcawston-v3.patch
```

`+` added, `-` removed, `~` rewritten under an unchanged name — the last being the
case a membership comparison calls identical. It needs a `patches` checkout carrying
both commits (`--patches-path`, else the config root's sibling `../patches`); without
one it reports "the commit moved, and here is why the files could not be listed"
rather than failing the rest of the comparison.

That section is what turns deciding whether a `validated` [support
claim](support-matrix.md) survives a kernel or patches bump from hand work into
reading a list.

## Bill of materials

```sh
# From a recipe's own published build.
boot2deb sbom turing-rk1/forky --format spdx --out turing-rk1-forky.spdx.json

# From an image someone handed you, by the provenance manifest that shipped with it.
boot2deb sbom ./turing-rk1-forky.provenance.toml --format cyclonedx

# Or write it as part of the build. Off by default; repeatable for both formats.
boot2deb build turing-rk1/forky --sbom spdx --sbom cyclonedx
```

**SPDX 2.3** and **CycloneDX 1.6**, both JSON, both from one internal model, so the two
documents state the same facts. What is in them:

| component | how it is identified |
| --- | --- |
| every installed package | name, exact version, sha256, and a `pkg:deb/debian/...` purl — carrying `&upstream=<source>` where the source package is named separately |
| every pinned source tree | the ref and the exact commit — kernel, u-boot, the patch series, and the media-accel trees |
| every rkbin blob | its sha256, which is the only identity it has |
| every externally-fetched `.deb` | its URL and sha256 |

The `upstream` qualifier is what ties the several binary packages of one source back to
the thing that was built — `libsystemd0`, `libsystemd-shared` and `systemd` are one
source package, and nothing else in the document says so. It comes from the published
plan beside the image, and it is emitted only where the source name *differs* from the
binary package's own, because that absence is how the ecosystem spells "the source
carries this name". An image handed over without its plan simply carries no attribution:
the SBOM is complete without it, and a warning says what is missing rather than the
command failing.

The one distinction worth reading is between what the image **contains** and what it was
**generated from**. A kernel source tree is compiled into the image, not installed in
it; SPDX says so with `CONTAINS` and `GENERATED_FROM`, and CycloneDX — which has one
relationship kind — carries it in the component type and description instead.

**Licenses are `NOASSERTION`, deliberately.** boot2deb records no per-package license,
and synthesizing one by reading `/usr/share/doc/*/copyright` out of the rootfs would
produce a field that looks authoritative and is not. An honest absence is worth more to
a compliance scan than a wrong SPDX identifier.

**The document is reproducible.** Its identity — the SPDX `documentNamespace` and the
CycloneDX `serialNumber` — is derived from the solved manifest's digest, so two SBOMs of
one package set are byte-identical rather than differing in a random UUID. The only
field the image's own content does not determine is the creation timestamp, which both
formats require; set `SOURCE_DATE_EPOCH` and the whole document is byte-stable.

It reads a *published build*, not a recipe's lock. A lock says what an image would be
made of; only a build says what one is — so `sbom <recipe>` reads the
`.provenance.toml` and `.pkgs.lock` beside that recipe's image, and says so if no build
has produced them yet.

## Where the size went

```sh
# The heaviest packages in a recipe's published image.
boot2deb size turing-rk1/forky

# Rolled up by the source package that produced them, all rows.
boot2deb size turing-rk1/forky --by source --top 0

# Debian's packages against the ones this build compiled into its own pool.
boot2deb size turing-rk1/forky --by archive

# Or from a plan someone handed you with an image.
boot2deb size ./turing-rk1-forky.plan --json
```

Answers "why is this image 2.1 GiB" from the plan document a build published — the one
file that carries each package's `Installed-Size` and `Source`.

```
size build/turing-rk1/forky/artifacts/turing-rk1-forky.plan — by source package

  1. systemd       21.7 MiB   35.5%     3 pkg
  2. glibc         19.2 MiB   31.4%     1 pkg
  3. file          10.6 MiB   17.3%     1 pkg

total 61.2 MiB across 8 packages in 6 source packages
```

Three axes, because three are answerable from a plan:

| `--by` | one row per | what it is for |
| --- | --- | --- |
| `package` (default) | binary package | what is biggest |
| `source` | source package | attributing a source's several outputs to the thing that was built |
| `archive` | repository | separating what Debian shipped from what this build compiled |

**A fourth — which config layer asked for a package — is not answerable**, and the
command does not pretend otherwise. The plan records the *repository* a package came
from, not the layer that named it, and most of an image is transitive dependencies no
layer named at all.

**The figures are the archives' own estimates, not measurements.** `Installed-Size` is
what each package's builder computed over a staged tree, in the kibibytes Debian Policy
defines it in. It counts no filesystem overhead, no inode shared by a hard link, and
nothing the image gains after `dpkg` — the initramfs, the `/boot` artifacts an install
hook produces, the ext4 metadata. So the total is smaller than the image, and the report
is for comparing rows against each other rather than for predicting a card's occupancy.
Policy also permits a package to state no size at all; those are counted apart rather
than folded in as zero, and the report says how many there were.

`--top` truncates the table and never the totals, so a partial view still states what it
is a view of; `--top 0` shows every row. `--json` prints the whole report — a consumer
that asked for structure can slice it itself.

## What has moved upstream

```sh
# Every recipe in the tree, in one pass.
boot2deb outdated

# Or just the ones you are about to touch.
boot2deb outdated turing-rk1/forky h96-max-m9/forky
```

`outdated` is the read-only sibling of `update`: it says what a re-pin *would* find,
without writing a lock. For each git source pin it reports one of

| status | meaning |
| --- | --- |
| `current` | the pinned tag is the newest comparable release, or the pinned branch is still at the pinned commit |
| `behind` | newer releases exist — the next one in the pin's own line, and the newest upstream |
| `tip-moved` | the pin names a branch whose tip has moved, with both commits |
| `unknown` | nothing could be compared, and why — a bare-commit pin, a ref the remote no longer advertises, or a remote that could not be reached |

```
recipe                        axis     status     detail
turing-rk1/forky              kernel   behind     v7.1.6 -> v7.1.9 (3 newer in this line); newest upstream v7.3 (7 newer)
turing-rk1/forky              u-boot   behind     v2026.04 -> v2026.07 (1 newer release, none in this line)
turing-rk1/forky              patches  tip-moved  branch main: tip moved 659033b7e543 -> ed52b7fa4a3d
```

Two figures because they are different moves. The **in-line** bump — the next stable
point release — usually keeps the patch series inside its declared `applies_to_kernel`
envelope and the kernel config where it was. The **newest upstream** release usually
does not, and is the one that wants a [`verify-patches`](#verify-patches) run before it
is pinned. A pin that is itself at the newest release in its line reports only the
wider move.

A release pin is never offered a **prerelease**: `v7.3-rc1` is not an upgrade from
`v7.1.6`, it is a different question. Nor is a pin compared across naming schemes — the
Linux-libre `sources/v7.1.6-gnu` trees and upstream's own `v7.1.6` live in the same
repo and their versions interleave, so a survey that mixed them would offer to swap a
board's whole firmware posture as a point release. The rule is that the pin states its
own scheme and only tags spelled the same way are candidates, which is why no per-axis
list of version patterns exists to fall out of date.

Being behind is not a failure. `outdated` always exits zero; it is a survey, and
whether to move is a decision with hardware evidence behind it. Its neighbour
[`verify-sources`](#verify-sources) is the gate, and it asks the opposite question —
not "is there something newer" but "is what we pinned still fetchable at all". A pin
can be a durable tag and nine releases behind, or an ephemeral branch tip and current.

Cost is one `git ls-remote` per **distinct remote**, not per pin: the shipped recipes
share a kernel repo and a patches repo, so surveying the whole tree is a handful of
round-trips and a few seconds. Nothing is fetched and nothing is written.

What it does **not** cover is the pins that have no upstream ref to move: the rkbin
blobs and any `extra_debs` are content-pinned by sha256 and read from the config tree,
and the apt archive is pinned by the solved manifest rather than by a ref. Those move
only when someone changes the config, which [`diff`](#comparing-two-build-points)
shows.

## Rebuild planning and cleanup

```sh
# Explain, offline, what the next build will actually redo.
boot2deb why-rebuild turing-rk1/forky

# Remove a recipe's build scratch to reclaim disk or force a clean rebuild. --dry-run
# previews; --cache / --sandbox / --build-roots clean only that subtree.
boot2deb clean turing-rk1/forky --dry-run

# Drop the provisioned build roots (sparing the packaging root) so the next build
# provisions them against the archive as it stands now.
boot2deb clean turing-rk1/forky --build-roots

# Sweep the caches every recipe shares — no recipe to name. The routine one drops
# the auto-fetched checkouts nothing pins any more, and verify-config's scratch.
boot2deb clean --verify-trees --kconfig --dry-run
```

`why-rebuild` answers the question that decides how long a build takes, and it answers
it for [both caches](#two-caches-and-what-each-one-keys-on). Per compile node it reports:

- whether the cloned-and-patched **source tree** is reused or rebuilt, naming the pinned
  input that moved when it will rebuild; and
- whether the **artifact cache** already holds that node's output, in which case the
  compile is skipped entirely.

The two are independent, and the second dominates. A node can rebuild its tree and still
compile nothing — the artifact store lives outside the work dir, so a fresh clone with no
tree at all can restore every `.deb`. Each verdict is computed by calling the same
function the build keys its own decision on, so the prediction cannot drift from what
happens next. It runs no build and touches no network.

```
why-rebuild turing-rk1/forky (work .../build/turing-rk1/forky)
  kernel             rebuild  (kernel.commit: kc1 → kc2)  [artifact cache hit — compile skipped]
  uboot              reuse
note: the per-node verdict is the *source tree*: whether the clone and patch run again.
      The compile itself is governed by the artifact cache ...
```

Pass `--no-artifact-cache` to see the prediction for a build that will not use the
store, and `--patches-path` / `--userspace <name>` to match a build that will use those.

`clean` removes only directories `build` created: every work dir is stamped with a
`.boot2deb-work` marker, and an unmarked target is refused — so a mistyped
`--work-dir` cannot recursively delete an arbitrary tree. `--force` overrides the
check for a directory you are sure about.

`--build-roots` is the narrow selector, and the one to reach for when a build fails
with **`the <stage> build root does not satisfy its own dependencies`**. A build root
is provisioned once and cached; the packages layered over it are resolved against the
archive as it stands when the build runs. The base's cache key covers the mirrors it
bootstrapped from and the package set it bootstrapped with — not the versions those
resolved to — so nothing invalidates the tree when the archive moves underneath it,
and an aged base cannot be told from a current one by inspection. Dropping it is what
clears the skew.

It sweeps the provisioned build roots, the `.lock` and `.pkgs` files beside each, and
the overlay layers staged over them, and it spares the **packaging root**. That root
is never layered — its contents are fixed at bootstrap — so it has no skew to hit, and
`--sandbox`, which takes it too, charges a second bootstrap for nothing. The two are
mutually exclusive for that reason: they answer opposite questions about the packaging
root. Preview either with `--dry-run` to see the trees and their sizes.

### Sweeping the shared caches

Four durable stores live under `<root>/cache`, outside every work dir, and they are
what a checkout accumulates over months rather than over one build. Because they are
shared, the selectors that name them take **no recipe**:

| selector | store | what makes it reclaimable |
| --- | --- | --- |
| `--verify-trees` | `cache/verify-trees`, `cache/patches` | the checkouts no lock pins |
| `--kconfig` | `cache/kconfig` | all of it — `verify-config` scratch |
| `--artifacts` | `cache/artifacts` | all of it — every entry is a compile away |
| `--all-caches` | `cache/` entire | all of it, pinned checkouts included |

`--verify-trees` is the routine one, and the only selector that prunes *within* a store
rather than emptying it. Both auto-fetch caches are keyed on the commit they hold, so
liveness is decidable: a checkout whose commit no `recipes/*/*.lock` names can only ever
be re-fetched, never read back from, and is dead. Re-pinning a kernel therefore strands
the old tree the moment `update` writes the new commit, and this is what collects it.
The still-pinned checkouts stay — they are what makes `verify-patches` and
`verify-config` start instantly. Because a *narrower* pinned set would delete a live
tree, a lock that will not parse aborts the sweep instead of narrowing it: nothing is
removed until every lock in the config tree has been read.

The one narrowing that rule cannot catch is a missing `--overlay`. The locks are read
from the search paths, so a sweep invoked without the overlays your builds use never
sees their pins and calls their checkouts dead. Pass the same `--overlay` flags you
build with; the run reports how many locks it read, so a count short of the tree you
know is the signal that something was left out.

`--kconfig` empties `verify-config`'s scratch, one work dir per recipe. Each holds a
provisioned cross root and an out-of-tree kbuild output dir, both re-created on the next
run, and each is a base that ages against the archive exactly as a build root does — so
dropping them costs a re-provision and buys back the largest of the four stores after
the artifact cache.

`--artifacts` empties the durable artifact store; since it is shared, that clears cached
outputs for every recipe, and the next build of each recompiles.

`--all-caches` takes the whole tree — the three above, the pinned checkouts, and the
pre-built `extra_debs` store. Everything there is reclaimable by construction, but
re-earning it costs a full re-fetch and a cache-cold rebuild, so it is the answer to
"I need the disk back", not to routine housekeeping.
