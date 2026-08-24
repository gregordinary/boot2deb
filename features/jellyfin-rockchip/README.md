# `jellyfin-rockchip` feature

Glue. It installs nothing; it seeds `/etc/jellyfin/encoding.xml` so that Jellyfin
transcodes on the Rockchip stack instead of in software.

```toml
# recipes/turing-rk1/jellyfin-forky.toml
features = ["jellyfin", "media-accel-rockchip", "jellyfin-rockchip"]
```

All three are named explicitly. `jellyfin` is portable and must not carry RK35xx
settings; `media-accel-rockchip` serves any consumer of the stack and must not
carry one application's config file. Neither can imply the other, so the binding
between them is its own feature.

## What the seed says, and why

### `EncoderAppPath` is the prefix path, not the `/usr/bin` symlink

`/opt/ffmpeg-rk/bin/ffmpeg`, never `/usr/bin/ffmpeg-rk`.

Jellyfin is never told where `ffprobe` is. It derives the path by replacing the
last component of the ffmpeg path with `ffprobe`. From the prefix path that gives
`/opt/ffmpeg-rk/bin/ffprobe`, which the `ffmpeg-rk` package ships. From the
`-rk`-suffixed symlink it gives `/usr/bin/ffprobe`, which this image does not
install — Jellyfin would start, encode, and fail every probe.

The prefix binary runs with no environment help: `ffmpeg-rk` carries
`DT_RUNPATH=/opt/ffmpeg-rk/lib` on every object it ships, and its build fails if
that RUNPATH did not reach the link.

### `HardwareAccelerationType` is `rkmpp`

It is the only value in Jellyfin's enum that reaches `h264_rkmpp` / `hevc_rkmpp`,
and those are the encoders that drive the VEPU580. `v4l2m2m` wants stateful V4L2
M2M nodes, which this kernel does not have — `rkvdec` is a stateless
(request-API) driver and encode goes through `mpp_service`. `vaapi` wants a VA
driver, and there is none for this SoC on mainline.

### `HardwareDecodingCodecs` is empty

This is required, not tuning, and it is the one setting an operator must not
"helpfully" re-enable.

The `*_rkmpp` decoders are compiled into `ffmpeg-rk`, so they appear in
`ffmpeg -hwaccels` and Jellyfin's capability probe — which reads exactly that —
concludes hardware decoding is available. It is not: MPP finds no decode client
on a mainline kernel and the decoder fails to open at runtime. Jellyfin cannot
see a runtime failure from a static probe, FFmpeg does not fall back to software
when a decoder fails to open, and Jellyfin does not retry without `-hwaccel`. The
stream simply fails.

Jellyfin's default for this field is `["h264", "vc1"]`, so leaving it unset would
break every H.264 transcode on this hardware. Empty makes Jellyfin emit no
`-hwaccel` at all: decode runs in software across eight cores, and the encoder
stays in hardware. On this board that is also the faster arrangement — RGA
scaling only pays for itself on frames already held in an MPP context, which on
this decode path they never are.

Hardware decode *is* available to FFmpeg here, as `-hwaccel v4l2request`. It is
not reachable from this file: Jellyfin's `HardwareAccelerationType` is a closed
enum with no `v4l2request` member.

An empty `<HardwareDecodingCodecs />` deserializes to an empty array rather than
`null` — .NET's generated array reader passes `isNullable: false` to
`ShrinkArray`, which returns a zero-length array for absent content. That matters
because Jellyfin calls `.Contains(...)` on the field without a null check.

### Tonemapping is off

`ffmpeg-rk` is built without OpenCL, so there is no hardware tone-map to reach
and enabling either tonemapping option would only add cost to a software path.

## Why `overlay-pre/` and not `overlay/`

Ownership.

Jellyfin rewrites `encoding.xml` on **every** start — `SetFFmpegPath()` stores the
resolved path back into the file — and it does so as user `jellyfin`, opening the
real path with `FileMode.Create`. There is no write-to-temp-and-rename, so
directory permissions do not save a file the service cannot open for writing. A
root-owned `encoding.xml` throws during startup.

Overlay trees are copied with `cp -a` and land root-owned, and the `jellyfin` uid
is allocated at install time, so the repo tree cannot pre-assign it. The
pre-install overlay solves this by handing the problem to the package that owns
it: `jellyfin-server.postinst` runs

```sh
if [[ $(stat -c '%u' $DIRECTORY) -eq 0 ]]; then
    chown -R ${JELLYFIN_USER}:adm $DIRECTORY
    chmod 0750 $DIRECTORY
fi
```

over `/etc/jellyfin`. A file laid down before the package is installed is inside
a still-root-owned directory when that runs, so it is chowned to `jellyfin:adm`
along with everything else. A file laid down after would keep `root:root` and
break startup.

This is what `preinstall_overlay_dirs` is for: config a package's maintainer
scripts must see while they run.

## Consequences worth knowing

- The seed is **starting values, not enforced values**. Jellyfin rewrites the
  file, so the dashboard wins from first boot onward, and the explanatory
  comments in the shipped file are gone after the first start. Change the
  defaults here for the next image; change the running server in its dashboard.
- The `jellyfin` feature installs no bundled FFmpeg (see its README), so
  `EncoderAppPath` is load-bearing. If it is wrong there is no encoder at all
  rather than a silent fallback to one that cannot reach the hardware.
