# `jellyfin` feature

An **application feature**: it installs the Jellyfin media server and carries no
transcode stack of its own. It is named for the app, so it composes with whatever
hardware-acceleration **capability feature** matches the target. It is `arm64`
only, and that gate comes from the vendored ICU `.deb` below rather than from
anything about the application.

## Composition (the "accelerated Jellyfin" use case)

The use case lives in a recipe, not in this feature's name:

```toml
# recipes/turing-rk1/jellyfin-forky.toml
device   = "turing-rk1"
features = ["jellyfin", "media-accel-rockchip", "jellyfin-rockchip"]
```

Three features, three jobs: the application, the transcode capability, and the
glue that points one at the other. On a different platform the same app feature
pairs with that platform's provider (a hypothetical `media-accel-vaapi` on
x86_64, `media-accel-nvenc` on NVIDIA) and that platform's glue. There is no
provider auto-resolution — the recipe names each one explicitly (non-goal).

One of those siblings is required, not optional, and that is **enforced**. This
feature installs no FFmpeg at all and Jellyfin will not start without a working
one — see [No bundled FFmpeg](#no-bundled-ffmpeg-and-what-that-costs) — so it
declares `requires_capability = ["ffmpeg"]` and a selection with no provider is
rejected at resolve rather than building an image whose server exits at boot:

```console
$ boot2deb resolve turing-rk1/forky+jellyfin
error: feature 'jellyfin' requires capability 'ffmpeg', which no selected feature
       provides — add one of 'media-accel-rockchip', 'media-accel-v4l2'
```

It names a *capability*, not the providers, so a provider for another platform
satisfies it with no edit here. The check validates the composition and does not
complete it — nothing is added to the selection, and the recipe still names every
feature.

## Packages

`jellyfin-server` and `jellyfin-web`, not the `jellyfin` metapackage.

The metapackage Depends on `jellyfin-server, jellyfin-web, jellyfin-ffmpeg7`, so
it drags in a second complete FFmpeg. `jellyfin-server` only *Recommends*
`jellyfin-ffmpeg7 | ffmpeg`, and this builder never installs Recommends, so
naming the two real packages keeps the bundled build out. That is what an image
supplying its own encoder wants: `jellyfin-ffmpeg7` is linked against the sonames
its own pocket carries, and pulling it onto a different Debian suite drags that
pocket's library versions in behind it.

## No bundled FFmpeg, and what that costs

With no bundled FFmpeg there is no fallback encoder, and on this application no
encoder means no server: Jellyfin resolves an FFmpeg path during startup,
validates that it runs, and `ApplicationHost` throws
`FfmpegException("Failed to find valid ffmpeg")` if it does not. It does not
start with transcoding switched off. So the encoder path is load-bearing, which
is a deliberate trade — a wrong path is a dead service rather than a silent
fallback to a binary that cannot reach the hardware — and it is the reason the
path is validated on hardware before a recipe using it stops being
`experimental`.

It also means Debian's own default is wrong for this image, and the feature has
to correct it. `jellyfin-server` ships

```sh
# /etc/default/jellyfin
JELLYFIN_FFMPEG_OPT="--ffmpeg=/usr/lib/jellyfin-ffmpeg/ffmpeg"
```

and `jellyfin.service` passes it on `ExecStart`. That path is `jellyfin-ffmpeg`'s,
so on this image it never exists. Jellyfin's resolution order is

1. the `--ffmpeg` command-line path,
2. `<EncoderAppPath>` in `/etc/jellyfin/encoding.xml`,
3. `ffmpeg` on `$PATH`,

and it takes the **first** one that is set, not the first one that works. So the
stock argument outranks any encoder a glue feature configures, and the server
dies before reaching it. Seeding `encoding.xml` is not sufficient on its own.

The feature's `overlay/` clears the argument, leaving step 2 to decide:

| file | role |
| --- | --- |
| `etc/systemd/system/jellyfin.service.d/no-bundled-ffmpeg.conf` | adds a second `EnvironmentFile=`, read after `/etc/default/jellyfin` |
| `etc/default/jellyfin-encoder` | sets `JELLYFIN_FFMPEG_OPT=""` |

Two files, for two reasons that are each easy to get wrong:

- **It cannot be an `Environment=` line in the drop-in.** systemd applies
  environment *files* after `Environment=` regardless of the order they appear
  in, so an `Environment=` would lose to `/etc/default/jellyfin`. A second
  `EnvironmentFile=` wins because files are read in the order listed and a
  drop-in is read after the unit it extends.
- **It does not edit `/etc/default/jellyfin`.** That file is a dpkg conffile, and
  so is the package's own `jellyfin.service.conf` drop-in. Rewriting either turns
  every `jellyfin-server` upgrade into a conffile conflict — which matters here
  because this feature deliberately leaves apt tracking Jellyfin's releases
  (see [Package source](#package-source)).

The drop-in's name sorts after `jellyfin.service.conf`, whose commented-out
`EnvironmentFile=` line invites an admin to uncomment it.

The value left for step 2 belongs to a glue feature, not to this one:
`jellyfin-rockchip` seeds `<EncoderAppPath>/opt/ffmpeg-rk/bin/ffmpeg</EncoderAppPath>`.
Leaving the argument empty rather than writing a path here also keeps the
dashboard's **FFmpeg path** field working — a command-line path would outrank
whatever an operator sets there, with no indication why.

## Package source

Jellyfin is not in the Debian mirror, so the feature adds its signed upstream apt
repository via `[[apt_sources]]`. The rootfs stage turns each resolved
`[[apt_sources]]` into a signed repository the bootstrap verifies against its own
keyring, so the packages and their dependencies resolve at bootstrap time rather
than in a post-install `dpkg -i` that resolves nothing.

The source **persists on the device** — its `sources.list.d` entry and its keyring
are written into the finished rootfs, so `apt upgrade` picks up Jellyfin's own
releases. That is deliberate for an application feature: a network-facing media
server that could only be updated by rebuilding and reflashing the image would be
a worse system than one that tracks its vendor's releases. (The build's own local
`.deb` pool is the opposite case and *is* removed before export — it is a
`file://` mirror under a temp directory that no longer exists once the image
runs.)

The repository signing key is a **build-host prerequisite**, vendored under
`blobs/keyrings/jellyfin.gpg` the same way the Debian archive keyring is —
see `blobs/keyrings/README.md`; a build whose declared source has no vendored
keyring fails fast before bootstrapping.

The declared suite is `trixie`, not the image's own codename: Jellyfin keys its
pockets on the base OS codename and publishes no `forky` pocket. The source
declares `main` only — the repository also publishes `unstable`, Jellyfin's
pre-release channel, which their install instructions enable so release
candidates are reachable.

## The vendored ICU, and the arch gate

Taking trixie packages onto a forky rootfs is not free. Jellyfin's packages are
self-contained .NET publishes that link the *system* ICU by soname, so
`jellyfin-server` declares `Depends: libicu76`. That is trixie's ICU; forky ships
`libicu78` and carries no `libicu76` at all, so the dependency cannot be
satisfied from the mirror.

An `[[extra_debs]]` entry supplies exactly that one library, pinned by sha256.
Nothing names it for install — `jellyfin-server`'s own `Depends` does, once the
bytes are in the local apt repo for the solve to find. Routing it through the
repo rather than a post-install `dpkg -i` is the whole point: the dependency is
*satisfied*, so the image's apt stays consistent. `libicu76` and `libicu78`
coinstall, which is what soname-versioned ICU packages are for.

Without it the failure is much larger than one dead service: the resolver drops a
dependency group it cannot satisfy, dpkg runs `--force-depends`, and
`jellyfin-server` installs and is marked configured — then FailFasts at startup
with "Couldn't find a valid ICU package installed", **and every subsequent
`apt install` on the device fails** on the unmet dependency. Measured on
hardware: an image that could not install `bc`.

This is where the feature's `arm64` gate comes from. `extra_debs` has no arch
templating, the local apt repo takes exactly one architecture, and a `.deb` whose
`Architecture` is neither the build's nor `all` fails the build rather than being
skipped — so the arch-pinned bytes decide the feature's arch. Supporting `amd64`
means a sibling feature carrying that arch's deb; everything else here is
arch-neutral and would be shared. Retarget both the suite and the ICU pin when a
`forky` pocket ships.
