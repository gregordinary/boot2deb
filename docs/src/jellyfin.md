# Accelerated Jellyfin

`turing-rk1/jellyfin-forky` and `turing-rk1/jellyfin-trixie` build a Turing RK1
image running the Jellyfin media server, transcoding video on the board's
hardware encoder.

```sh
boot2deb build turing-rk1/jellyfin-forky
```

Flash it the way you would any other RK1 image — see
[Turing RK1](boards/turing-rk1.md) — and Jellyfin comes up on port 8096 with the
transcode settings already filled in. There is nothing to configure to get
hardware encoding; there is one setting you should not change, described below.

## What is accelerated, and what is not

**Encoding runs on the VEPU580. Decoding and scaling run on the CPU.**

That is the whole shape of it, and it is not the shape Jellyfin's dashboard
implies, so it is worth being plain about. Jellyfin offers an "Enable hardware
decoding for" list alongside the hardware-encoding switch; on this image that list
is deliberately empty.

The reason is that the two halves of Rockchip's stack are in different states on a
mainline kernel. The `*_rkmpp` **encoders** talk to `mpp_service` and work. The
`*_rkmpp` **decoders** expect an MPP decode client, and mainline does not provide
one — `rkvdec` is a V4L2 stateless driver instead. The decoders are still compiled
in, so they appear in `ffmpeg -hwaccels`, and Jellyfin's capability probe reads
exactly that list and concludes hardware decoding is available. It is not. Turning
a codec on in that list makes Jellyfin emit `-hwaccel rkmpp`, the decoder fails to
open, and the stream fails — FFmpeg does not fall back to software when a decoder
cannot open, and Jellyfin does not retry without it.

So: leave *Playback → Transcoding → Enable hardware decoding for* empty. Everything
else in that page is yours to tune.

On this board that is also the faster arrangement rather than a concession. RGA
scaling only pays for itself on frames already held in an MPP context, which on a
software-decode path they never are; the round trip to and from the 2D engine costs
more than swscale saves. Eight Cortex cores decode and scale, and the encoder — the
part that actually would not keep up in software — is in hardware.

FFmpeg on this image *can* decode in hardware, with `-hwaccel v4l2request`. Jellyfin
cannot be pointed at it: its acceleration type is a fixed list with no
`v4l2request` in it.

## What the image sets up for you

| Setting | Value | Why |
| --- | --- | --- |
| FFmpeg path | `/opt/ffmpeg-rk/bin/ffmpeg` | The only build here that can reach the VEPU580 |
| Hardware acceleration | `rkmpp` | The only type that reaches `h264_rkmpp` / `hevc_rkmpp` |
| Hardware encoding | on | |
| Hardware decoding | *(empty)* | See above — leave it empty |
| Tone mapping | off | This FFmpeg is built without OpenCL |

These are written to `/etc/jellyfin/encoding.xml` before first boot. They are
**starting values**: Jellyfin rewrites that file on every start, so from first boot
onward the dashboard is what governs. To change the defaults for the next image,
edit `features/jellyfin-rockchip/overlay-pre/etc/jellyfin/encoding.xml` in your
config tree.

HEVC output is left off, as it is in stock Jellyfin — whether your clients can play
HEVC is a fact about your household, not about the board. `hevc_rkmpp` is there and
works if you turn it on.

## Jellyfin's bundled FFmpeg is not installed

The image installs `jellyfin-server` and `jellyfin-web`, not the `jellyfin`
metapackage, so `jellyfin-ffmpeg` never lands. It would be a second complete FFmpeg
that cannot reach the hardware, and it pulls its own pocket's library versions in
behind it.

The consequence is worth knowing: there is no fallback encoder, and on this
application no encoder means no server. Jellyfin validates the FFmpeg path during
startup and exits if the binary does not run — it does not start with transcoding
switched off. So if you point it at a path that does not exist, the service dies at
boot. Check with `journalctl -u jellyfin`; the giveaway is
`Failed to find valid ffmpeg`. If you want the bundled build available as a safety
net, add `jellyfin-ffmpeg7` to a copy of the `jellyfin` feature's package list.

**Set the path in the dashboard, not on the command line.** The image ships a
`jellyfin.service` drop-in that clears the `--ffmpeg=` argument Debian normally
passes, precisely so that Jellyfin reads the path from its config — which is what
**Dashboard > Playback > Transcoding > FFmpeg path** edits. Putting a path back on
the command line (by editing `/etc/default/jellyfin-encoder`) would override that
field and leave the dashboard silently ineffective.

## Keeping it updated

Jellyfin's own apt repository stays configured on the running system — the image
writes its `sources.list.d` entry and keyring — so `apt upgrade` picks up Jellyfin
releases the ordinary way. Debian's mirrors are there too. Nothing about this image
requires a reflash to take a security update to the server.

The exception is `ffmpeg-rk`. It is built from source, pinned by commit in the
recipe's lock, and comes from no repository, so `apt upgrade` will never move it.
It is also the component that parses untrusted media. Moving it means re-pinning and
rebuilding:

```sh
boot2deb outdated turing-rk1/jellyfin-forky   # what has moved upstream
boot2deb update   turing-rk1/jellyfin-forky   # re-pin
boot2deb build    turing-rk1/jellyfin-forky
```

## Where the media lives

The recipes declare no data volume, on purpose — an RK1 running Jellyfin might keep
its library on an M.2 disk, an external drive, or network storage, and the recipe
cannot know which. To attach one, add the `data-volume` feature and a
`[[data_volumes]]` block to your own copy of the recipe; see
[Data volumes](data-volumes.md).

## Checking it is working

Play something that must be transcoded — a file in a codec the client cannot take,
or with subtitles burned in — and look at the FFmpeg command Jellyfin logged:

```sh
sudo grep -h "ffmpeg" /var/log/jellyfin/*.log | tail -1
```

You want to see `/opt/ffmpeg-rk/bin/ffmpeg`, `-c:v hevc_rkmpp` or `h264_rkmpp`, and
**no** `-hwaccel rkmpp`. If `-hwaccel rkmpp` is there, a codec got re-enabled in the
hardware-decoding list.

Every run also prints `mpp_platform: client N driver is not ready!` for a handful of
N. That is normal: libmpp probes for vendor client types a mainline kernel does not
have. The ones that matter (RKVENC, RKVENC_CCU, RKVDEC, JPEG_DEC) are present.

## Status

Both recipes are `experimental`, and the gap is Jellyfin rather than the hardware
underneath it.

The transcode path itself is measured on a boot2deb-built RK1 image. `h264_rkmpp` and
`hevc_rkmpp` produce correct streams — every frame of a 90-frame clip in both codecs,
from software frames and through `hwupload` alike, verified against a stock FFmpeg on
another machine rather than against the build that produced them. Hardware decode
through `-hwaccel v4l2request` cuts decode CPU cost by 53x at 1080p and up to 143x at
4K, and HEVC decode is bit-exact against software.

What has not been done is driving that path *from Jellyfin* on the board: playing a
file through the server and confirming the transcode it launches is the accelerated
one. Until that happens, treat the settings above as configured rather than proven.
See the [support matrix](reference/support-matrix.md) for what each recipe has been
taken through.
