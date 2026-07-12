# Adding a patch

boot2deb applies an ordered **patch series** to each source tree (kernel, ffmpeg,
userspace, u-boot) before it compiles. The series is declared by a
[patch profile](../reference/config-model.md#patch-profiles-belong-to-the-kernel) that
lives in the separate `patches` repo. Adding a patch means getting it into that series
and then into a build. This page walks the loop end to end.

It applies to a kernel that names a profile. A kernel with `patch_profile = "none"`
applies no series and never reads the `patches` repo; giving such a board a patch means
first authoring a profile for its kernel.

## The loop

```text
patch import  ->  commit in ../patches  ->  boot2deb update  ->  boot2deb build
                          (verify-patches at any point along the way)
```

The linchpin is the middle two steps: **`update` re-pins the `patches` repo's current
commit into the lock, and `build` reads the series at exactly that pinned commit.** So a
patch sitting on disk does nothing until it is *committed* in the patches repo and the
lock is *re-pinned* to include that commit. `patch import` prints these follow-ups for
you; the rest of this page is the same steps, with their failure modes.

The running example imports a kernel patch into the `rk3588-accel` profile and builds the
`turing-rk1-forky` recipe.

## 1. Import the patch

`patch import` fetches a patch (a patchwork/mbox URL, a local file, or `-` for stdin),
normalizes it to canonical `git am`-ready mbox, writes it into the profile's tree, and
slots it into the profile manifest at the right position:

```sh
cargo run -p boot2deb-cli -- patch import \
  https://patchwork.kernel.org/project/linux-rockchip/patch/NNNN/mbox/ \
  --profile rk3588-accel --scope kernel
```

- `--scope` selects which tree's series to insert into: `kernel`, `ffmpeg`, `userspace`,
  or `uboot`.
- The filename prefix is chosen to sort the patch at its list position; pass `--position`
  to insert at a specific index (default: append). If the neighbours leave no numeric gap
  (e.g. `070`/`071`), the import falls back to a lettered sub-prefix (`070a`)
  automatically.
- Add `--verify-tree <kernel-checkout>` to dry-run `git am` the resulting series during
  the import (it rolls back the write on failure). Without it, the patch is written
  unverified — see [verify](#3-verify).

On success it prints exactly what to do next:

```text
patch import: wrote media-accel/kernel/045-fix-foo.patch (3812 bytes)

!! patch written but NOT verified — it has not been dry-run against a kernel tree.
   verify it now:   boot2deb verify-patches turing-rk1-forky
                    (auto-fetches the locked kernel at its pin — no checkout needed)
   next time:        add --verify-tree <kernel-checkout> to verify during import.
patch import: rk3588-accel/kernel now lists the patch at position 5 of 11

next steps — no build reads the patch until the series is committed and re-pinned:
  1. commit it:      git -C ../patches add -A && git -C ../patches commit
  2. re-pin locks:   boot2deb update turing-rk1-forky
```

The re-pin line names each recipe whose kernel uses the profile you imported into.

## 2. Commit in the patches repo

The new patch file and the profile edit both live in the `patches` repo. Commit them
there:

```sh
git -C ../patches add -A
git -C ../patches commit -m "kernel: fix foo"
```

This matters because `update` pins the patches repo's **HEAD commit**. An uncommitted
patch is invisible to that pin — which is exactly why `update` refuses to run against a
dirty patches checkout (see [failure modes](#failure-modes)).

## 3. Verify

Confirm the series still applies cleanly to the pinned kernel:

```sh
cargo run -p boot2deb-cli -- verify-patches turing-rk1-forky
```

With no `--kernel-path`, `verify-patches` **auto-fetches the locked kernel at its pin** —
no hand-cloned tree needed. The first run on a cold cache clones linux-stable (large); if
you already have a checkout, point `--kernel-src` at it to skip the clone:

```sh
cargo run -p boot2deb-cli -- verify-patches turing-rk1-forky --kernel-src ../linux
```

You can verify before or after committing — the series on disk is what is checked.
(Passing `--verify-tree` to `patch import` runs the same check inline.) See
[Verification](../reference/cli.md#verification) for `verify-config` and the full flag
set.

## 4. Re-pin the lock, then build

`update` re-pins the patches commit (and re-resolves the other refs) into the lock:

```sh
cargo run -p boot2deb-cli -- update turing-rk1-forky
```

You do **not** need `--kernel-ref` for a patch-only re-pin: with a lock already present,
`update` inherits the previous kernel ref and re-pins only what changed. Commit the
updated `recipes/<recipe>.lock`, then build:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky
```

The build reads the series at the pinned commit and applies it with `git am --3way`.

## Failure modes

**Dirty patches checkout.** `update` refuses a `patches` repo with uncommitted changes
(`PatchesDirty`): a dirty pin would be wrong in every case, so commit first. This guard
turns "I imported but forgot to commit" into an instant, offline error instead of a
confusing build-time one.

**Stale / mismatched pin.** If the checkout `build` reads is at a different commit than
the lock pins, you get `PatchesPinMismatch`. Its remedy text distinguishes the cases: if
your local HEAD is *ahead* of the pin (you committed but did not re-pin), run `update`;
if the checkout is *behind* the pin (stale), `git checkout` the pinned commit. Re-pinning
after a commit is the usual fix.

**Auto-fetch can't find the commit.** A zero-clone build (no local `../patches`) fetches
the series at the pinned commit from the profile's `patches_url`. That only works if the
commit has been **pushed** — an unpushed local commit resolves fine against a local
checkout but not on another machine. Push the patches repo before relying on the
auto-fetch.

## Co-developing the series

While iterating on a patch you may not want to commit-and-re-pin on every change. Point
`build` (and `verify-patches`) at your working checkout instead:

```sh
cargo run -p boot2deb-cli -- build turing-rk1-forky --patches-path ../patches
```

An explicit `--patches-path` **downgrades a pin mismatch from an error to a loud
warning**, so you can build from an uncommitted, un-re-pinned series. The trade-off is
reproducibility: that build is no longer pinned by the lock, so it is a development
convenience, not a committed result. When the patch is settled, commit it, `update`, and
drop the flag.
