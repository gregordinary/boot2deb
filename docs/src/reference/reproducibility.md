# Reproducibility

Reproducibility here is **a property of a lock, not a promise of the tool.** boot2deb
does not guarantee that any clone rebuilds any image forever — that would over-promise,
and during active development it is not even true. What it guarantees is narrower and
honest: the machinery to make *a given build point* reproducible to whatever strength
you choose, and a documented way to rebuild the images the project publishes.

A build is a point across axes (device, kernel, suite, features, layout). The recipe
`.toml` records the *constraints*; the sibling `.lock` records the *exact resolution* —
every pinned commit, blob hash, and package manifest. `build` reads only the lock. That
separation is what lets one recipe serve two intents without choosing between them:

- **Rolling** — "give me a current working image." Fresh clone, `update` to re-pin at
  today's upstream, `build`. Best day-to-day UX; the resulting image's provenance records
  exactly what went into it, so it is reproducible *as of now*.
- **Frozen** — "reproduce exactly what shipped." The lock is pinned and left alone; the
  image ships with a provenance manifest, and rebuilding it is a mechanical replay.
  Reproducible *across time*.

You opt into a strength per lock. Rolling and frozen are the same tool at two dial
settings.

## The three layers

An image rests on three independent inputs, each with its own durability and its own
way to pin. Reproducibility is only as strong as the weakest one you froze.

### 1. Upstream sources (git commits, blobs)

Every compiled input — kernel, u-boot, the MPP/RGA/ffmpeg trees — is pinned to an exact
commit in the lock; rkbin blobs are pinned by sha256. A commit is only re-fetchable if
its remote still advertises it, so pins fall into durability classes: a **release tag**
is immutable and fetchable forever; a **branch tip** is ephemeral (a force-push orphans
it); a **bare local commit** is unfetchable by construction. boot2deb keeps shipped
recipes on durable tags, makes a non-durable pin loud at `update` time, and never
substitutes a different commit for an orphaned one — a different SHA is different bytes.

`boot2deb verify-sources <recipe>` is the check: a read-only probe that reports each pin
as `durable | ephemeral | ORPHANED | skipped` and exits non-zero on any orphan, so CI can
gate on it. It touches only the git remotes.

**Custom kernels.** A custom kernel is pinned the same way — a source commit plus a patch
series commit. Its one failure mode is rebasing or force-pushing the patch repo, which
orphans the pinned commit. Keep it in the durable class by **tagging the patch repo at
each release**; the pinned commit then lives under an immutable ref and stays fetchable
across future rebases.

### 2. The Debian archive (rootfs)

The rootfs is the fast-moving layer: a testing suite like `forky` changes daily, and the
exact package versions a build installs rotate off the live mirror as it advances. Three
mechanisms pin it:

- The lock's solved manifest fixes **which bytes** install — every package name, version,
  and sha256. This is always present.
- The published **plan document** (`<point>.plan`, beside the image) additionally fixes
  *how to install exactly those bytes*, and records the archive state they were selected
  from. `boot2deb reproduce` replays it. This is written by every build.
- A captured `snapshot.debian.org` timestamp fixes **availability** of those bytes after
  they leave the live mirror. This is opt-in and dormant by default (`mode = off`), so
  day-to-day builds go straight to the live mirror.

The manifest and the plan are not two spellings of one thing. The manifest is a *pin*
the next build is verified against — a fresh solve that no longer reproduces it is an
error. The plan is an *instruction*: hand it back and the rootfs installs that set
without solving at all, which is the difference between detecting drift and not being
subject to it. And the plan carries what neither the manifest nor the lock does — the
mirror that answered, the suite and components, the sha256 of the release body that was
verified, its `Date` and `Valid-Until`, the fingerprint of the certificate that verified
it, and the key under that certificate which made the signature. The provenance manifest
repeats those as `[[archives]]`, one entry per repository, so an image's own record says
what its packages were selected *from* and not only which they were.

The plan document states its own format version in its first line, and a boot2deb reads
exactly one. A plan written by an older boot2deb is **refused, naming both versions**,
rather than being read on a guess about what its fields meant:

```console
$ boot2deb reproduce turing-rk1/forky --from published/
error: read the pinned plan published/turing-rk1-forky.plan: the document is in format
"ferroday-cage-plan 1", and this library reads "ferroday-cage-plan 2"
```

That is the correct failure and not a gap to work around. The version moved because a
field changed *meaning*: `Signed-By` named the signing subkey and now names the
certificate's primary key. Reading an old document under the new rule would silently
report a different key for an archive that never rotated one — which is exactly the event
a recorded fingerprint exists to catch. Rebuild the image from its lock to get a plan in
the current format.

Replaying a plan **moves the trust anchor**, which is why it is `reproduce`'s to do and
not something a `build` flag can turn on. A pinned install reads neither a release nor a
package index, so the package digests no longer chain to an archive signature; they chain
to the plan. Each `.deb` is still verified against the digest the plan records, so a
mirror serving different bytes is caught — what is no longer checked is that the document
describes a set the archive ever offered. For reproducing an image you published, whose
plan you have alongside it, that is the right trade. For a routine build it is not, and a
build that sets no plan resolves exactly as before.

One consequence is worth stating plainly: **a recipe that compiles its own packages
replays only if those compiles are byte-reproducible.** The kernel `.deb`, and on a
media-accel recipe `ffmpeg-rk`/`librockchip-mpp1`/`librga2`, install from the build's own
local pool and are pinned by digest like everything else. So a replay either matches
them — which proves the whole image reproduced, compiles included — or fails naming the
package that drifted. The failure is the honest outcome, not a defect in the mechanism: a
build that cannot reproduce its own compiler output was never reproducible, and this is
where that becomes visible instead of silent. A board that installs Debian's kernel and
compiles nothing has no such dependency.

Snapshot has three modes: `off` (live mirror only), `fallback` (live first, the snapshot
backfills anything that 404s), and `pin` (the snapshot only — a fully deterministic
userland). Capture a timestamp with `--save-snapshot`; activate a mode with `--snapshot
fallback|pin`. A `fallback`/`pin` with no captured timestamp is refused rather than
silently downgraded.

The mirror list a mode resolves to is used for **every root a build provisions**, not
only for the image's own userland: the target-arch sandbox the media-accel packages
compile inside, the host-arch cross root the kernel, u-boot and modules compile inside,
and the packaging root whose `dpkg-deb` archives them. Those roots hold the compilers and
the archiver, so pinning the runtime without them would fix what ships and leave what
produced it free to move. Each root's identity is folded into the artifact-cache keys of
what it built, so a snapshot-pinned build never restores a live-mirror build's `.deb`s —
and so is the build-dependency set layered over it, because a compile probes for what is
present.

This is why forky's churn is **not** at odds with the model: the tool to freeze against it
exists; a frozen build turns it on.

### 3. The builder (boot2deb itself)

The same lock built by a different boot2deb can produce a different image, or fail to read
an old lock — during active development, breaking changes are expected, and the project
does not carry compatibility shims to read old locks forever. So the builder is an input
like any other, and the provenance manifest records it in `[built_with]`.

That section carries **two** commits, because "what produced this image" has two answers
that move independently:

| field | names | captured |
|---|---|---|
| `commit` / `dirty` | the *program* — the boot2deb binary that ran | stamped into the binary when it is compiled |
| `config_commit` / `config_dirty` | the *data* — the config tree it read layers, recipes and the lock from | read from `--root` when the build starts |

One checkout can supply both, and in the layout boot2deb is developed in it does. They
still cannot answer for each other: an installed `boot2deb` run against a config tree has
a `commit` from wherever it was built and a `config_commit` from the tree in front of it,
and a single binary building two different config trees is the ordinary case, not an
exotic one.

The binary's commit is stamped at **compile** time rather than read at run time, and that
is deliberate. The binary *is* the builder, so its identity has to travel with it: an
installed boot2deb has no source tree in reach, and reading whatever checkout happened to
be nearby would record a different claim than the field makes. The cost is that a binary
can fall behind the checkout it was built from — commit, forget to `cargo build`, and the
next image is stamped with the commit *before* yours, or with one an amend left
unreachable.

So a build refuses to start when the running binary provably is not this checkout's
source:

```console
$ boot2deb build h96-max-m9/forky
error: this boot2deb was compiled from 7e6e2f02674c, but the checkout is at
90ab9c660bc1. An image built now records 7e6e2f02674c as its builder — a commit
that is not what is on disk, and that nobody else can resolve if it was amended
away. Run `cargo build` (seconds) to re-stamp it.
```

Two cases are certain enough to refuse: the stamp names a different commit than `HEAD`,
or it names `HEAD` but the sources under `crates/` have been edited since. Editing a
device `.toml` or a `.dts` is *not* one of them — that is build input, recorded by the
lock and by `config_commit`, and it leaves the binary's identity intact. A binary
compiled from an already-dirty tree is reported and not refused: it records
`dirty = true`, which is the honest answer rather than a false one.

`--allow-stale-builder` proceeds anyway, for when you mean it. The check itself is two
`git` reads, and it runs before any compile — the alternative is discovering the wrong
stamp in a provenance file written at the *end* of the build. `boot2deb doctor` reports
the same verdict standing still, and `why-rebuild` shows it as a `builder` row above the
compile nodes.

The builder also decides the environment a compile runs in. Every package build and the
rootfs customize run in an unprivileged sandbox, and what they produce depends on the
variables they carry, the filesystem they see, the identity they hold, whether they can
reach a network, and which syscalls succeed — none of which any source pin covers, and all
of which move with the sandbox library boot2deb links. So the manifest records them as data
rather than leaving them to be inferred from a version: `[sandbox]` is the launch posture,
`[sandbox_env]` is the command's complete environment, and `[[sandbox_mounts]]` is every
mount the sandbox establishes, in order, down to the `/dev` device nodes and symlinks. Two
images built from one lock that differ can be compared on the inputs that could explain it.

That record is the series every command *starts from*. A run's own working and artifact
binds are per-build paths, its root is a per-build path, and the subordinate identity map
the rootfs customize adds is its own — so none of them is recorded. The rooting mode
contributes its *kind* (`plain` or `overlay`) and nothing else, for the same reason: a
record carrying an overlay's lower stack or a range map's id extents would be a property of
the machine rather than of the builder.

That stamp is an **as-built record, not a requirement.** The stamped commit is a *floor*:
it, and later commits up to the next change that alters the output for this lock, will
reproduce the image — and a later one may carry fixes you want. A commit past that change
will not. And the floor is all that can ever be recorded, because the breaking change is
in the future and unknowable at build time — even a bugfix can be output-affecting. So the
stamp says *when the build worked*, never *when it will break*. A reproduce flow reads it
to **advise** — "built with X; you are on Y, newer, likely fine; here is how to get X" —
never to enforce.

## The build host

The three layers above are inputs you choose. The build *host* is not — it is whatever
machine you happened to run on — so the rule for it is different: **a host setting either
does not reach the image, or it is recorded.** Nothing in between.

What is kept out:

- **Your umask.** Git records two file modes and no directory modes at all, so a checked-out
  overlay tree's modes are your umask, not authored data. The staged tree is normalized back
  to git's own model — directories `0755`, files `0644` or `0755` by the executable bit —
  before it is laid into the rootfs. Without it a `002` umask (the Ubuntu/Pop!\_OS default)
  ships a group-writable `/etc`, `/usr`, and `/boot`, and a `077` umask ships an image whose
  `/etc` no non-root process can read.
- **Your git configuration.** Every `git` the build runs, and the pure-Rust clone beside it,
  is isolated from `/etc/gitconfig`, `~/.gitconfig`, and `/etc/gitattributes`. The setting
  that decides this is `url.<base>.insteadOf`: it rewrites a remote URL, so a host carrying
  one would fetch a pinned commit from a remote the lock does not name — the exact input the
  lock exists to fix. `core.hooksPath`, `am.threeWay`, `apply.whitespace`, and a system
  `gitattributes` are the same class. Transport settings are the cost: express a proxy or
  credentials through the environment (`http_proxy`, `https_proxy`), which git still reads.
- **Your distro's `dpkg`.** No `.deb` is archived by a tool from your host, the kernel's
  included. The u-boot and kmod packages are staged, then archived by a `dpkg-deb` from a
  **packaging root**; the kernel's `make bindeb-pkg` runs `dpkg-buildpackage` and
  `dh_builddeb` inside the **cross root**. Both are Debian userlands resolved from the
  same mirror list as the image itself, so the archiver's version and its `liblzma` are
  sha256-pinned inputs the lock describes rather than a property of the distribution that
  ran the build. The compressor for what boot2deb archives itself (`xz`, level 6) is
  stated rather than inherited on top of that, so the archive's structure is a property of
  boot2deb and not of the suite. No `fakeroot` on any path either — every root maps the
  caller to uid 0, so a staged tree is already root-owned where it is archived and
  `dpkg-buildpackage` needs no gain-root command.
- **Your compiler.** Every compile runs in a provisioned root: the kernel, u-boot and the
  out-of-tree modules in a host-arch **cross root** carrying `crossbuild-essential-<target>`,
  and the media-accel `.deb`s in a target-arch **build sandbox**. Neither your `gcc` nor
  your `make` is on any build path, and the host cross toolchain is not either. Each stage
  additionally *declares* the build-dependencies it layers over that base, and the
  declaration is folded into the artifact key — because a compile probes for what is
  present, and a package added to the layer is a different build.
- **Your `TMPDIR`.** The provisioned rootfs — the whole target userland, carrying xattrs
  and mapped ownership — is staged in the build's work dir. On `/tmp` it would land on a
  RAM-backed `tmpfs` on most desktops, making "does the build fit" a property of your
  mount table.
- **Your shell environment.** Every build command runs with `TZ=UTC` and `LC_ALL=C.UTF-8`,
  and with `KCFLAGS`/`KAFLAGS`/`KCPPFLAGS`/`MAKEFLAGS` cleared, so a flag exported in your
  shell cannot shape kernel bytes that a lock-keyed cache entry claims to reproduce.
- **Your `openssl`.** The image's first-boot `/etc/shadow` entry is hashed in-process. No
  host binary sits on the credential path.

What is recorded, because it genuinely does reach the image:

- **`[toolchain]`** — the host/target arch and the cross prefix. `jobs` records the
  parallelism: recorded but deliberately not keyed, since a build whose output depends on
  its job count has a bug, and keying it would fragment the artifact cache by machine
  size. The compilers are not here; they are named, sha256-pinned, in the root sections
  below.
- **`[toolchain.qemu]`** — the `qemu-user` interpreter that, on a host that cannot
  execute target binaries, ran the target compiler for the sandbox-built packages *and*
  every maintainer script that configured the rootfs. Absent where nothing is
  interpreted; an arm64 host building armhf cross-compiles and then runs the result
  natively, so it records none. This is the one compile input still probed on the host,
  because it is registered with the host kernel's binfmt handler and no provisioned root
  can carry it.

  It is taken from **the kernel's binfmt registration**, not from a `PATH` lookup, and
  the difference is not academic: the registered path is normally a wrapper under
  `/usr/libexec/qemu-binfmt/` rather than the `qemu-<arch>-static` on your `PATH`, and
  nothing requires the two to name the same file. A build with no interpreter on `PATH`
  at all still runs every target binary through the registered one. So `interpreter` is
  the path the kernel recorded, `resolved` is that path with symlinks followed — the two
  are separate facts, because repointing the wrapper symlink swaps the interpreter with
  the registration unchanged — and `sha256` is the content, which is also what the
  artifact cache keys on. A digest rather than a version line because it moves when the
  binary is rebuilt at an unchanged version, and because it can be taken from a binary
  that refuses to run, which the wrapper name does. `version` is read from the resolved
  path for a reader, and may be absent.
- **`[filesystem]`** — the on-disk contract the rootfs was formatted to. Every other pin
  answers "which sources went in"; this one answers "what shape were they written into",
  and it is the only such determinant that moves independently of the lock, since the
  format options are builder constants rather than resolved config values. It is three
  records, because three things move for three different reasons:

  - `policy_pin` is the **intent** — the formatter's own policy document, carried whole:
    every feature word twice over, as exact bits and as names, plus the block and inode
    sizes, plus the seven options outside the feature set entirely (the grow reservation,
    the inode ratio, the reserved share, the error behaviour, the journal size, and the
    two directory-hash choices). Every one of those moves bytes, and `errors` is the sharp
    case: it reaches neither a feature word nor the geometry, so no other record here
    would notice it changing. Nothing image-specific is in it — no UUID, no timestamp, no
    label, no block count — so two images built from these constants carry byte-identical
    policy pins, and a difference always means the contract changed.
  - `reference_geometry_pin` is what that policy **lays out**, planned at one size chosen
    once (4 GiB) and never moved. It closes the gap the policy pin cannot see: a change to
    the *formula* behind an option whose name did not change. `grow max` reads the same
    before and after a change to what `Max` reserves; the blocks it reserves do not. It is
    a function of the options and the reference size alone, so it says nothing about what
    went into the image.
  - `[filesystem.geometry]` is what the format **realized for this image** — block and
    inode counts, group layout, and `max_grow_blocks`, the ceiling the reserved descriptor
    blocks buy, which is how large a disk the image can still grow onto at first boot. It
    answers to the image's size as well as to the policy, so a larger partition moves
    every number in it with both pins unchanged.
- **`[verification]`** — which checks the finished rootfs filesystem passed. The built-in
  scan always runs — every metadata checksum, each group's metadata placement, and every
  in-use inode's block map, directory records and attributes — and any finding at all
  fails the build. The independent `e2fsck -fn` cross-check runs only where the host
  carries `e2fsprogs`; its value is not extra depth (the scan is deeper) but independence,
  since the scan is one implementation checking its own output. That makes verification
  *depth* host-determined, so it is stated rather than left to a log line, and a release
  build can be gated on it.
- **`[image].image_bytes`** — the whole-disk size the build laid out, beside the
  `image_size` the recipe authored. The two agree for a stated size and the redundancy is
  the point; they differ in kind for a measured one, where `fit+20%` states the rule and
  only this says what it came to. Without it a fitted image's manifest could not answer
  how large its own image is.
- **`[[archives]]`** — the state each configured repository was in when the rootfs plan
  resolved, in the order the resolve saw them, which is the index the `.plan` document's
  packages name. Per entry: the mirror that answered, the suite and components, the sha256
  of the release body that was verified, its `Date` and `Valid-Until`, and two key
  fingerprints. `signed_by` is the **certificate** that verified the release — its primary
  key, which is what a keyring entry is named by and what `blobs/keyrings/*.fingerprints`
  pins, so the manifest is directly comparable against that list. `signing_key` is the key
  that actually made the signature, usually a dedicated signing subkey of that
  certificate; the two are separate because a certificate rotating its subkey moves the
  second and leaves the first alone. `[rootfs]` says which package bytes shipped; this says
  what they were selected *from*, which is the question a solved manifest cannot answer —
  the same suite resolves to different versions a week apart. An empty `signed_by` is a
  fact rather than a gap: it says that repository was trusted unsigned, which is how the
  build's own `.deb` pool is configured, and `signing_key` is empty exactly when it is.
  That pool's entry is marked `local` and carries no mirror, because its URL is a per-run
  path under a per-run directory — a property of the machine, kept out for the same reason
  the sandbox record carries no working or artifact path.
- **`[build_sandbox]`, `[cross_sandbox]` and `[packaging_root]`** — the package sets of the
  three provisioned roots that produced the build's `.deb`s: the target-arch base that
  *compiled* the media-accel packages, the host-arch base that *compiled* the kernel,
  u-boot and out-of-tree modules, and the host-arch root whose `dpkg` *archived* the staged
  ones. `[rootfs]` records what the image *carries*; these record what *produced* the parts
  of it boot2deb built — further Debian trees, resolved from the same mirrors, that no
  source pin covers. Each names a manifest published beside the image
  (`<recipe>.sandbox.pkgs`, `<recipe>.cross.pkgs`, `<recipe>.packaging.pkgs`), sha256-pinned
  per package exactly as the rootfs manifest is. `[cross_sandbox]` in particular is where
  the compiler is named, by package and sha256 rather than by the version string it
  prints — which is why `[toolchain]` above carries no `cc`. Each is absent when the build
  produced nothing of its kind — no cross root for a board that installs Debian's kernel
  and boots its own firmware, no packaging root for a build whose artifacts all came back
  from the artifact cache. They are records, not contracts: nothing pins them in the lock
  and no later build is verified against them.
- **`[sandbox]`, `[sandbox_env]` and `[[sandbox_mounts]]`** — the posture, the environment
  and the complete mount series every sandboxed build command runs under, as the sandbox
  library resolves them. All three sit outside that library's compatibility promise, so
  they are recorded rather than inferred from its version, and most of what they hold has
  no other accessor at all — down to the `/dev` device nodes and symlinks.

  `[sandbox]` states how the sandbox is rooted (`plain` or `overlay`), the identity the
  command holds (`single` — the calling user is root inside and nothing else is mapped),
  the network it can reach (`isolated` — a fresh namespace with loopback only, declared by
  boot2deb rather than taken from a library default), where the three standard streams go,
  any resource limits in force, and whether the library's hardening layer is compiled in.
  `hardening = "unavailable"` is written rather than omitted: an absent key cannot be told
  from one written before the key existed, and a provenance record has to be readable
  without knowing which builder wrote it.

  `[sandbox.streams]` is there because a build's output depends on it: `isatty` on the
  standard streams steers debconf's frontend, a compiler's colour diagnostics, and every
  progress display, so two builds under two stream postures can differ with nothing else to
  show for it. boot2deb declares `stdin = "null"`, which does more than state that a build
  is non-interactive — it puts the sandboxed command in a **session of its own**. Inherited,
  it would stay in yours, where a maintainer script could open `/dev/tty` to read what is
  typed at your terminal and push characters into its input queue for your shell to run
  afterwards. Out of that session `/dev/tty` fails. The output pair reads `inherit`, which
  is a statement about the profile rather than about a compile: a capturing launch attaches
  its own pipes, so build output never went to your terminal either.

These identities also key the caches, so a `.deb` built with one toolchain is never
restored for a build using another — and neither is a rootfs whose packages were configured
under a different `qemu-user`.

## Two audiences

Because reproducibility is a property of a lock, the story splits by who owns the lock.

**The project, publishing a release.** We own every axis — recipe, lock, snapshot
timestamp, patch-repo tag, builder commit — so we offer a *closed* guarantee for a shipped
image: check out boot2deb at the stamped commit, build this lock, get that image. The
consumer mostly flashes; rebuilding is the frozen path. This is the release ritual below.

**Someone who clones and authors their own recipe.** Their subject is *their* build point,
not ours, and their reproducibility is forward-looking — "make my current build re-buildable
later" — rather than "rebuild what the project shipped." They own their lock: when to
`update`, whether to `--save-snapshot`, which builder they are on. The project does not
guarantee their build; it hands them the *same machinery* and lets them set the strength.

## The release ritual

To publish an image that stays reproducible across time, freeze all three layers and commit
the result:

1. **Freeze the userland:** capture a `snapshot.debian.org` timestamp into the lock with
   `boot2deb build <recipe> --save-snapshot` and set its mode to `pin`, so the rootfs is
   deterministic even after the suite advances. Commit the snapshot-pinned lock — it is part
   of the release.
2. **Keep sources durable:** tag the patch repo at its pinned commit, and confirm
   `boot2deb verify-sources <recipe>` reports no `ORPHANED` pins.
3. **Build from that clean, committed checkout**, so the image's `[built_with]` records
   real commits with `dirty = false` and `config_dirty = false`. Run `cargo build` first:
   the build refuses a binary that is behind the checkout, but nothing can make a
   *dirty* one identify itself, and a release stamped `dirty = true` names no commit
   anyone can return to.
4. **Publish the image together with its `.provenance.toml` and its `.plan`.** The
   manifest names the builder that produced it and the archives it resolved against; the
   plan is the document that replays them; the committed lock — recoverable at that
   commit — carries the snapshot timestamp and every source pin.
5. **Ship a bill of materials with it**, for the consumers who read one rather than a
   provenance manifest: `--sbom spdx --sbom cyclonedx` on the build, or
   [`boot2deb sbom`](cli.md#bill-of-materials) later from the manifest in step 4. It is
   deterministic on the same terms as everything else here — its identity is derived
   from the solved package set, so set `SOURCE_DATE_EPOCH` and two renderings of one
   image are byte-identical.

## Reproducing a frozen image

```sh
boot2deb reproduce <recipe> --from <dir holding the published .plan>
```

That is the whole flow. It runs the ordinary pipeline — the lock's pinned commits and
blobs reproduce the compiled inputs — and replaces one step: the rootfs installs the
plan's exact package set instead of solving for a new one. Point `--from` at wherever the
image, its provenance manifest and its `.plan` were published; omit it to use this build
point's own output directory, which is where a build on this machine already wrote them.

The command reads the `[built_with]` stamp beside the plan and reports how the running
checkout compares. That is advice, not a gate — the stamp is a floor, not a ceiling: a
newer builder usually reproduces the image too and may carry fixes, so a current clone is
the normal first attempt, and `git checkout <built_with.commit>` is the step to take only
if it diverges. The builder stamp lives in the build's `.provenance.toml`, not on the
image; the on-image `/etc/boot2deb/image.toml` (see [Image identity](image-identity.md))
records the image and kernel identity, which a rescue tool reads without the provenance
file.

What each layer contributes to that one command: the **lock** reproduces the sources, the
**plan** reproduces the package set, and the lock's **snapshot pin** keeps that set
fetchable after the live mirror has moved on. Freeze all three and the replay is
mechanical; freeze fewer and it is reproducible to whatever strength you chose.

## What is deliberately outside the claim

The per-image first-boot password is unique per build by design, so `/etc/shadow` is
intentionally not byte-reproducible. Everything else in the rootfs is, given the same three
layers frozen. The rootfs export clamps every tar member's mtime to `SOURCE_DATE_EPOCH`, so
a bootstrap's wall-clock stamps do not leak into the image: its encoder records each mtime
as `min(mtime, epoch)` as it writes. The
encoder is the one place that can apply the ceiling: under the subordinate id-map that gives
the tree its real ownership, the provisioned files sit at ids the host user cannot set times
on. The export also emits entries in sorted order — directory children and extended attributes
by name — so a content-identical tree encodes to a byte-identical archive.
