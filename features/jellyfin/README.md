# `jellyfin` feature

An **application feature**: it installs the Jellyfin media server and is
portable — no `requires_soc` / `requires_arch` gate, no transcode stack of its
own. It is named for the app, so it composes with whatever hardware-acceleration
**capability feature** matches the target.

## Composition (the "accelerated Jellyfin" use case)

The use case lives in a recipe, not in this feature's name:

```toml
# recipes/turing-rk1-jellyfin.toml
device   = "turing-rk1"
features = ["jellyfin", "media-accel-rockchip"]
```

On a different platform the same app feature pairs with that platform's provider
(a hypothetical `media-accel-vaapi` on x86_64, `media-accel-nvenc` on NVIDIA).
There is no provider auto-resolution — the recipe names both features explicitly
(non-goal).

## Package source

Jellyfin is not in the Debian mirror, so the feature adds its signed upstream apt
repository via `[[apt_sources]]`. The rootfs stage renders each resolved
`[[apt_sources]]` into a signed `deb [signed-by=…]` positional for the
`mmdebstrap` solve, so apt resolves `jellyfin` and its dependencies at bootstrap
time — installed at build, not left as a live third-party repo on the
device. The repository signing key is a **build-host prerequisite**, vendored under
`blobs/keyrings/jellyfin.gpg` the same way the Debian archive keyring is —
see `blobs/keyrings/README.md`; a build whose declared source has no vendored
keyring fails fast before bootstrapping.

## Not shipped yet (follow-ons)

- **Overlay wiring** — an overlay could point Jellyfin at the system HW ffmpeg
  (`ffmpeg-rk`, from `media-accel-rockchip`) via `/etc/default/jellyfin`. That
  glue is Rockchip-specific and depends on runtime paths validated on hardware,
  so it rides the physical boot gate rather than shipping speculatively.
