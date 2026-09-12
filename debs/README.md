# debs

Pre-built `.deb`s a build consumes that no Debian suite carries, referenced by
`[[extra_debs]]` with a `path` locator and pinned by sha256. `boot2deb update`
verifies each file against its hash and copies it into the content store; a build
materializes from that store, so the pin is what is trusted and the file here is
only where the bytes came from.

An entry's `targets` decide where the bytes go. `image` is the local apt repo the
rootfs solves against; `ffmpeg` is the ffmpeg stage's build pool, which is how a
library ffmpeg links but Debian does not ship reaches `./configure`.

## The AVS2 and AVS3 decoders

`libdavs2` decodes AVS2 and `libuavs3d` decodes AVS3. FFmpeg wraps both — as
`libdavs2` and `libuavs3d` — and Debian packages neither, so `ffmpeg-rk` cannot
be configured against them from the archive.

- `libdavs2-16` / `libdavs2-dev` — AVS2, built from upstream
  <https://github.com/pkuvcl/davs2> at `b41cf11` over Deepin's `debian/`
  packaging (<https://github.com/deepin-community/davs2>) at `843424c7`.
  **GPL-2 or later**, which is what lets it combine with an
  `--enable-version3` FFmpeg. Built `--disable-asm`: davs2 carries no aarch64
  assembly at all, so the flag is what makes it compile here, and the packaged
  library correctly contains zero NEON symbols.
- `libuavs3d1` / `libuavs3d-dev` — AVS3, built from upstream
  <https://github.com/uavs3/uavs3d> at `0e20d2c` with four packaging patches.
  **BSD-3-Clause**, and it does carry real aarch64 assembly — 107 NEON symbols
  in the packaged library.

Both come from the `avs-debian` src2deb recipe, which holds the packaging, the
patches and the upstream pins; these files are its arm64 forky output. Because
`libdavs2` is GPL-2+ **and built here rather than consumed from Debian**, the
source offer has to be answerable: the pins above are what a request is served
from.

Only the runtime libraries reach an image. The `-dev` packages are build inputs
and target the ffmpeg stage alone; the dbgsym packages and Deepin's `davs2` CLI
are not vendored at all.
