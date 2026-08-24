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
exact package versions a build installs rotate off the live mirror as it advances. Two
mechanisms pin it:

- The lock's solved manifest fixes **which bytes** install — every package name, version,
  and sha256. This is always present.
- A captured `snapshot.debian.org` timestamp fixes **availability** of those bytes after
  they leave the live mirror. This is opt-in and dormant by default (`mode = off`), so
  day-to-day builds go straight to the live mirror.

Snapshot has three modes: `off` (live mirror only), `fallback` (live first, the snapshot
backfills anything that 404s), and `pin` (the snapshot only — a fully deterministic
userland). Capture a timestamp with `--save-snapshot`; activate a mode with `--snapshot
fallback|pin`. A `fallback`/`pin` with no captured timestamp is refused rather than
silently downgraded.

The mirror list a mode resolves to is used **twice**: for the image's own userland, and
for the target-arch sandbox the media-accel packages compile inside. That sandbox holds
the `gcc` those `.deb`s are built with, so pinning one without the other would fix the
runtime and leave the compiler free to move. The sandbox identity is folded into those
packages' artifact-cache keys too, so a snapshot-pinned build never restores a
live-mirror build's `.deb`s.

This is why forky's churn is **not** at odds with the model: the tool to freeze against it
exists; a frozen build turns it on.

### 3. The builder (boot2deb itself)

The same lock built by a different boot2deb can produce a different image, or fail to read
an old lock — during active development, breaking changes are expected, and the project
does not carry compatibility shims to read old locks forever. So the builder is an input
like any other, and the provenance manifest records it: a `[built_with]` section with the
boot2deb version, the git commit of the checkout that built the image, and whether that
checkout was dirty.

The builder also decides the environment a compile runs in. Every package build and the
rootfs customize run in an unprivileged sandbox, and what they produce depends on the
variables they carry and the filesystem they see — neither of which any source pin covers,
and both of which move with the sandbox library boot2deb links. So the manifest records
them as data rather than leaving them to be inferred from a version: `[sandbox_env]` is the
command's complete environment, and `[[sandbox_mounts]]` is every mount the sandbox
establishes, in order, down to the `/dev` device nodes and symlinks. Two images built from
one lock that differ can be compared on the inputs that could explain it.

That record is the series every command *starts from*. A run's own working and artifact
binds are per-build paths, and the `/etc/resolv.conf` an `apt` run binds comes from the
build host, so neither is recorded — the section describes the builder, not the machine.

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
- **Your distro's `dpkg` defaults.** The host-built `.deb`s state their compressor (`xz`,
  level 6) rather than inheriting it, so the same recipe at the same lock produces the same
  archive on a Debian host and an Ubuntu-derived one — the latter defaults `dpkg-deb` to
  `zstd`.
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

- **`[toolchain]`** — beyond the host/target arch and cross prefix, the version lines of the
  `gcc`, `as`, and `ld` that compiled the kernel, u-boot, and out-of-tree modules, and the
  `qemu-user` that, on a cross host, executed the target compiler for the sandbox-built
  packages *and* every maintainer script that configured the rootfs. The compiler
  is the largest host input to the kernel bytes and no source pin covers it, so an image's
  provenance can answer "which gcc built this". Each is absent when the build compiles
  nothing that needs it. `jobs` records the parallelism: it is recorded but deliberately not
  keyed, since a build whose output depends on its job count has a bug, and keying it would
  fragment the artifact cache by machine size.
- **`[verification]`** — which checks the finished rootfs filesystem passed. The built-in
  reader check (every metadata checksum) always runs; the independent `e2fsck -fn`
  cross-check runs only where the host carries `e2fsprogs`. That makes verification *depth*
  host-determined, so it is stated rather than left to a log line, and a release build can
  be gated on it.
- **`[build_sandbox]`** — the package set of the sandbox base that compiled the build's
  target `.deb`s (the media-accel packages). `[rootfs]` records what the image *carries*;
  this records what *produced* the parts of it boot2deb compiled — a second Debian tree,
  resolved from the same mirrors, that no source pin covers. It names a
  `<recipe>.sandbox.pkgs` manifest published beside the image, sha256-pinned per package
  exactly as the rootfs manifest is, and is absent from a build that compiles no target
  `.deb`s and therefore stands up no sandbox. It is a record, not a contract: nothing pins
  it in the lock and no later build is verified against it.
- **`[sandbox_env]` and `[[sandbox_mounts]]`** — the environment and the complete mount
  series every sandboxed build command runs under, as the sandbox library resolves them.
  Both sit outside that library's compatibility promise, so they are recorded rather than
  inferred from its version, and the mounts have no other accessor at all — down to the
  `/dev` device nodes and symlinks.

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
3. **Build from that clean, committed checkout**, so the image's `[built_with]` records a
   real commit with `dirty = false`.
4. **Publish the image together with its `.provenance.toml`.** The manifest names the
   builder that produced it; the committed lock — recoverable at that commit — carries the
   snapshot timestamp and every source pin.

## Reproducing a frozen image

1. Read the published `.provenance.toml` for the `[built_with]` commit that produced it.
2. `git checkout <built_with.commit>` in a boot2deb clone — this recovers the recipe and the
   snapshot-pinned lock exactly as they were at build time.
3. `boot2deb build <recipe>` — the lock's snapshot pin makes the userland deterministic, and
   the pinned commits and blobs reproduce the compiled inputs.

The stamp is a floor, not a ceiling: a newer builder usually reproduces the image too and may
carry fixes, so a current clone is the normal first attempt — step back toward the stamped
commit only if it diverges. The builder stamp lives in the build's `.provenance.toml`, not on
the image; the on-image `/etc/boot2deb/image.toml` (see [Image identity](image-identity.md))
records the image and kernel identity, which a rescue tool reads without the provenance file.

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
