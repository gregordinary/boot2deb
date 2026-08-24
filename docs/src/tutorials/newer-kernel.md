# Moving a board to a newer kernel

A board that compiles its own kernel is pinned to an exact tag, and that tag goes stale.
This tutorial moves the Turing RK1 forward, in the two shapes that job takes:

- **Within the track** — `v7.1.1` to a later `7.1.y` point release. The patch series
  already claims the version, so this is a re-pin and a rebuild.
- **Across a version boundary** — 7.1 to 7.2. The series makes no claim about 7.2 yet, so
  the first move is *measuring* whether it would survive, without changing a pin.

The second shape is the interesting one, and the order it runs in is the point: find out,
then adopt. Nothing mutates a lock or a series until the answer is in.

> **This is the build side.** Upgrading the kernel *on a board that is already running* is
> a different job with a different mechanism — see [Upgrading the
> kernel](../kernel-upgrades.md).

## What a kernel bump touches

Four things, in different repos, and they move at different rates:

| Thing | Where | Moves when |
| --- | --- | --- |
| The pinned tag | the recipe's `.lock` | every bump |
| The kernel definition (`track`, fragments, series) | `kernels/<id>.toml` | at a version boundary — a kernel definition is version-coupled |
| The series envelope and per-patch ranges | the `patches` repo | when a boundary is measured |
| The `[support]` claim | the recipe | when the moved pins are re-earned on hardware |

The device's `supported_kernels` list gates which definitions a board may resolve, so a new
definition is also one line there.

## Shape 1: within the track

`kernels/rk3588-mainline-7.1.toml` tracks `7.1.y`, and `series/rk3588-accel.toml` declares
`applies_to_kernel = ">=7.1.5, <7.2"`. A later 7.1 point release is inside both, so if the
series holds, nothing in the config or the series changes — only the pin.

```sh
# 1. Re-pin. This is the only command that consults upstream.
boot2deb update turing-rk1/forky --kernel-ref v7.1.5

# 2. Does the series still apply to the new tree? (--kernel-src makes the fetch
#    near-instant if you have a checkout; omit it and the tree is auto-fetched.)
boot2deb verify-patches turing-rk1/forky --kernel-src ../linux

# 3. Does the .config still generate cleanly on the patched tree?
boot2deb verify-config turing-rk1/forky

# 4. What will actually recompile? Offline, reads the lock and the build stamps.
boot2deb why-rebuild turing-rk1/forky

# 5. Build.
boot2deb build turing-rk1/forky
```

Step 1 says two things worth reading rather than scrolling past. Because
`turing-rk1/forky` claims `validated`, moving its pins retires the evidence that claim
rested on, and `update` says so:

```
  warning: recipe 'turing-rk1/forky' claims support = "validated" as of 2026-07-16, but
  this update moved its pins:
    kernel v7.1.1 (c9acdc466e9a) -> v7.1.5 (155b42bec9cb)
  that claim now describes a combination nothing has booted — re-validate on hardware and
  update the date, or set status = "expected" until you do
  note: this re-pin changes the generated support matrix — regenerate it:
    boot2deb support-matrix --markdown > docs/src/reference/support-matrix.md
```

Both are advisory — the lock is written either way — and both name work that belongs at the
end of this tutorial, in [Closing the loop](#closing-the-loop).

Deciding whether the `validated` claim survives means knowing *what* the re-pin moved,
which is what `diff` is for. Keep the old lock before step 1 and compare against it
afterwards:

```sh
cp recipes/turing-rk1/forky.lock /tmp/old.lock       # before step 1
boot2deb diff /tmp/old.lock turing-rk1/forky         # after it
```

It names the kernel ref and commit that moved, the kconfig symbols the fragment sets
now request differently (with the fragment behind each), and — where the patches
commit moved too — the individual patch files that were added, removed, or rewritten.
That is the evidence the claim is re-earned or retired on. See
[Comparing two build points](../reference/cli.md#comparing-two-build-points).

If you would rather not touch the lock until you know the answer, step 2 can come first:
`verify-patches turing-rk1/forky --kernel v7.1.5 --kernel-path ../linux` measures a version
the lock does not name and leaves the lock alone. That is the [candidate
path](#step-2-measure-and-change-nothing) below, and it works inside the envelope as well as
outside it.

A patch that fails at step 2, or a kconfig symbol that has been renamed out from under a
fragment at step 3, turns this shape into the next one: the series has hit a boundary, even
inside its declared envelope.

## Shape 2: across a version boundary

### Step 1: get a tree at the candidate version

The lock pins no commit for a kernel it does not name, so the candidate has to come from a
checkout you point at:

```sh
git -C ../linux fetch --tags origin
git -C ../linux checkout v7.2
```

A release candidate is a legitimate answer here — measuring `v7.2-rc5` before 7.2 exists is
exactly what this path is for.

### Step 2: measure, and change nothing

```sh
boot2deb verify-patches turing-rk1/forky \
    --kernel v7.2 --kernel-path ../linux --keep-going
```

`--kernel` verifies against a kernel the lock does not pin and **leaves the lock alone**.
Three rules differ from the locked path, and each of them exists so the question can be
asked at all:

- **The declared envelope does not gate the run.** The series still says `<7.2` — refusing
  the candidate on that basis would answer the question by assuming it. The run reports
  that the kernel is outside the envelope and measures it anyway, and what `git am` does is
  the answer. A clean result is the *evidence for* widening the envelope, not a claim that
  it already covers 7.2.
- **A release candidate is matched as its base release**, because by semver `7.2.0-rc3`
  satisfies neither `<7.2` nor `>=7.2` and a release-only range would reject every RC. The
  build path stays release-strict; this path does not.
- **`--keep-going` reports every failure in one pass.** One boundary usually spawns
  adjacent ones — a reworked patch shifts the context every later patch applies against —
  so stopping at the first turns the survey into serial discovery. Each failing patch is
  skipped so the rest still get measured, which makes the report a map of the damage rather
  than a final verdict.

Per-entry `kernels` ranges still narrow the series on this path, so a patch already marked
obsolete at the candidate drops out instead of counting as a failure.

### Step 3: act on the report

Every failure is one of three things, and each has its own encoding in the series manifest:

**Upstreamed.** The patch is in 7.2 already; the failure is the code being there twice.
Give the entry an upper bound rather than deleting it — an older kernel still needs it:

```toml
kernel = [
  { path = "rocket/084-rocket-drv-fix-bo-mm-uaf.patch", kernels = "<7.2" },   # in mainline as of 7.2
]
```

**Reworked.** The patch is still needed but no longer applies. Rebase it, keep both
versions, and give them complementary ranges — one list then builds 7.1 and 7.2 correctly
from a single checkout, which a list mutated in place cannot do:

```toml
kernel = [
  { path = "media-accel/kernel/050-av1-iommu-v14-curated.patch", kernels = "<7.2" },
  { path = "media-accel/kernel/050-av1-iommu-v15-curated.patch", kernels = ">=7.2" },
]
```

Regenerate a rebased patch with `git format-patch` rather than hand-editing the diff —
`git am --3way` needs the index lines a hand-written hunk does not carry.

**Obsolete.** Nothing needs it any more, at any version. Retire the entry and its file:
an old lock names an old `patches` commit, whose tree still contains both.

Re-run step 2 after each round. When it comes back clean, and only then, widen the
envelope:

```toml
applies_to_kernel = ">=7.0, <7.3"
```

That edit is what turns a measurement into a claim, which is why it comes last. Commit it in
the `patches` repo — and push it, because a pin naming an unpushed commit resolves against
your checkout and nowhere else:

```sh
git -C ../patches add -A && git -C ../patches commit -m "rk3588-accel: extend to 7.2"
git -C ../patches push
```

### Step 4: write the kernel definition

A kernel definition owns everything version-coupled, so 7.2 is a new file rather than an
edit — `kernels/rk3588-mainline-7.2.toml`:

```toml
flavor           = "mainline"
source           = "linux-stable"
track            = "7.2.y"
base_defconfig   = "defconfig"
config_fragments = ["base/debian-arm64", "soc/rk3588", "accel/full"]
patch_series     = ["rk3588-accel"]
patches_url      = "https://github.com/gregordinary/patches.git"
supported_socs   = ["rk3588"]
```

The series name is unchanged — series names are semantic, never version-suffixed, so the
kernel definitions referencing them stay stable. Then let the board resolve it, in
`devices/turing-rk1.toml`:

```toml
supported_kernels = ["rk3588-mainline-7.1", "rk3588-mainline-7.2"]
default_kernel    = "rk3588-mainline-7.1"        # still the validated one
```

Check the point resolves before pinning anything — `--kernel` selects the definition for a
resolve without touching a file:

```sh
boot2deb resolve turing-rk1/forky --kernel rk3588-mainline-7.2
```

### Step 5: adopt it in a recipe

`update` has no `--kernel` flag: which definition a recipe pins is the recipe's own
`kernel` field, not a per-run choice, so adopting 7.2 is a recipe edit. Do it in a **new
leaf** rather than in `forky.toml`, and the board keeps a validated recipe while the new one
is unproven — `recipes/turing-rk1/forky-7.2.toml`:

```toml
device   = "turing-rk1"
kernel   = "rk3588-mainline-7.2"
suite    = "forky"
features = []
layout   = "combined"

[support]
status = "experimental"     # nothing has booted this yet
date   = "2026-07-29"       # the day the claim was last assessed
```

Then run the same sequence as shape 1 against the new leaf, on the locked path this time —
no `--kernel`, because the lock now names 7.2 itself:

```sh
boot2deb update         turing-rk1/forky-7.2 --kernel-ref v7.2
boot2deb verify-patches turing-rk1/forky-7.2 --kernel-src ../linux
boot2deb verify-config  turing-rk1/forky-7.2
boot2deb build          turing-rk1/forky-7.2
```

If you skipped ahead and pinned 7.2 before widening the envelope, nothing is broken: the
envelope check is pure metadata, so `update` reports the mismatch and keeps going (pinning
is the first step of adopting), while `build` refuses before cloning anything and names the
`verify-patches --kernel` line to run instead. That is the cheap check telling you the
series makes no claim about your kernel, ahead of the expensive one that tells you whether
it would have worked anyway.

### Step 6: the kernel config

A version bump moves kconfig too: symbols get renamed, split, or absorbed, and a fragment
naming one that no longer exists silently stops setting anything. `verify-config` reports
the merge, and with a reference config it asserts byte-identical `CONFIG_*` parity:

```sh
boot2deb verify-config turing-rk1/forky-7.2 \
    --reference-config build/turing-rk1/forky/linux/.config
```

Comparing the new kernel's generated config against the old one's is the fastest way to see
what the bump changed on its own. Expect differences and read them; the ones that matter are
options you *asked* for and did not get.

### Step 7: boot it

Everything so far is a build-time claim. Flash the image, boot the board, and check the
hardware the kernel is there for — for the RK1, see [Turing RK1](../boards/turing-rk1.md).

## Closing the loop

A kernel bump is not finished when the image builds:

1. **Re-earn the claim.** Set `status = "validated"` and the date on which an image from
   these pins booted. A claim is per-pin: it cannot be re-dated for pins that moved, only
   re-earned. Until then, `expected` is the honest status.
2. **Regenerate the matrix**, which is generated from the locks and therefore stale the
   moment a pin moves:
   ```sh
   boot2deb support-matrix --markdown > docs/src/reference/support-matrix.md
   ```
3. **Check the pins are durable.** `verify-sources` grades every pin in the lock — a tag is
   durable, a branch tip is ephemeral, a force-pushed branch is orphaned:
   ```sh
   boot2deb verify-sources turing-rk1/forky-7.2
   ```
4. **Retire the old definition** once the new one is validated: delete the superseded
   recipe leaf and kernel file, and drop the id from `supported_kernels`. Old locks name old
   commits in the `patches` repo, so retiring the config does not strand a build that was
   already pinned.

Keeping both kernel definitions is worth it only while a board genuinely supports both.
