# `jellyfin` feature

An **application feature**: it installs the Jellyfin media server and is
portable — no `requires_soc` / `requires_arch` gate, no transcode stack of its
own. It is named for the app, so it composes with whatever hardware-acceleration
**capability feature** matches the target.

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

Composed alone, Jellyfin installs and runs and transcodes in software.

## Packages

`jellyfin-server` and `jellyfin-web`, not the `jellyfin` metapackage.

The metapackage Depends on `jellyfin-server, jellyfin-web, jellyfin-ffmpeg8`, so
it drags in a second complete FFmpeg. `jellyfin-server` only *Recommends*
`jellyfin-ffmpeg8 | ffmpeg`, and this builder never installs Recommends, so
naming the two real packages keeps the bundled build out. That is what an image
supplying its own encoder wants: `jellyfin-ffmpeg8` is linked against the sonames
its own pocket carries, and pulling it onto a different Debian suite drags that
pocket's library versions in behind it.

With no bundled FFmpeg there is no fallback encoder. An image that gets the
encoder path wrong has no transcoding rather than transcoding on a binary that
cannot reach the hardware — a deliberate trade, and the reason the encoder path
is validated on hardware before a recipe using it stops being `experimental`.

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

The declared suite is `trixie`, not the image's own codename. Jellyfin keys its
pockets on the base OS codename and publishes no `forky` pocket; the trixie
packages install and run on a forky rootfs. The source declares `main` only —
the repository also publishes `unstable`, Jellyfin's pre-release channel, which
their install instructions enable so release candidates are reachable.
