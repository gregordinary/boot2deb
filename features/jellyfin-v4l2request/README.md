# `jellyfin-v4l2request` feature

Jellyfin decoding in hardware as well as encoding. It builds the Jellyfin server from
source with the `patches/jellyfin` series, and seeds the configuration that pairs the
two halves: decode on `rkvdec` through the V4L2 request API, encode on the VEPU580
through MPP.

```sh
boot2deb update turing-rk1/forky+media-accel-rockchip+jellyfin+jellyfin-v4l2request+vulkan+avs-decode
boot2deb build  turing-rk1/forky+media-accel-rockchip+jellyfin+jellyfin-v4l2request+vulkan+avs-decode --image-size 3G
```

It takes the place of `jellyfin-rockchip`, and the two cannot be combined. Both seed
`/etc/jellyfin/encoding.xml`, which the server reads and rewrites as one document, so
one feature owns it.

## Why the server is patched

A stock Jellyfin server decides decode and encode with one setting,
`HardwareAccelerationType`. On an RK3588 with a mainline kernel the two halves are on
different stacks. Decode is `rkvdec`, a V4L2 stateless driver, and encode is the VEPU580
through MPP. The `rkmpp` type names the encoder correctly and a decoder that cannot open
here, so a stock server has to leave hardware decode off.

The series adds `HardwareDecodingType` and `HardwareEncodingType`, which override the
one setting per half, and a decode-only `v4l2request` type. With them the server emits
`-hwaccel v4l2request` in front of `hevc_rkmpp` or `h264_rkmpp`. The patches and their
measurements are in `patches/jellyfin/README.md`.

## What the seed says

| Setting | Value |
| --- | --- |
| `EncoderAppPath` | `/opt/ffmpeg-rk/bin/ffmpeg` |
| `HardwareAccelerationType` | `rkmpp` |
| `HardwareDecodingType` | `v4l2request` |
| `HardwareEncodingType` | `rkmpp` |
| `EnableHardwareEncoding` | `true` |
| `HardwareDecodingCodecs` | `h264`, `hevc`, `vp9` |
| Tone mapping | off |

The codec list is what `rkvdec` decodes on the RK3588. AV1 is a separate `hantro-vpu`
decoder with known gaps on some streams, so it is left for an operator to add. The
`v4l2request` type declines 10-bit content itself, so a 10-bit stream in a listed codec
decodes in software and still encodes in hardware.

The file is an `overlay-pre/` seed for the reason `jellyfin-rockchip`'s is:
`jellyfin-server`'s postinst hands `/etc/jellyfin` to the service user, and only a file
laid in before the package installs is inside that sweep. See
`features/jellyfin-rockchip/README.md`.

## How the server is built

The feature declares the server as an app (`[[apps]]`), and the build compiles it in the
`app:jellyfin` node:

1. Jellyfin's packaging tree (`jellyfin-packaging`) is checked out at the tag that
   packaged the server's release, and patched with the series' `app_packaging` scope.
2. The server tree is checked out at its release tag inside it, at `jellyfin-server/`,
   and patched with the series' `app` scope.
3. The SDK and every NuGet package the restore needs are fetched, verified against their
   pins, and laid into an offline feed.
4. `dpkg-buildpackage -B` builds `jellyfin-server` in the host-architecture cross root,
   with no network.

`update` pins everything the build reads: both trees, the series, the SDK tarballs, and
the NuGet packages, which one networked restore resolves into a sidecar beside the lock.
The config model page covers the mechanism.

## Held at its release

The built `jellyfin-server` replaces the one Jellyfin's repository serves, and two things
keep it in place:

- **Its version carries an epoch** (`1:12.1+g…`). The rootfs solve takes the highest
  version across its repositories, so it takes this build over any release the
  repository publishes later. A device's `apt upgrade` does the same.
- **It depends on the `jellyfin-web` of its own release.** The solve installs the
  matching web client, and `apt upgrade` holds the web client back rather than break
  that dependency.

The repository stays configured, so everything else it serves still updates. Moving the
server to a new Jellyfin release is a config change: the app's `ref` and the packaging
`ref` in `features/jellyfin-v4l2request.toml`, then `update` and `build`.
