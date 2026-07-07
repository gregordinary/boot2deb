# blobs

Vendored Rockchip `rkbin` firmware the u-boot build consumes, one directory per
SoC (`blobs/<soc>/`). Each blob is referenced by filename in the device/soc
config and pinned by sha256 in the recipe lock; the build verifies the file
against the lock's hash before u-boot consumes it. Vendoring keeps builds
reproducible and offline.

## `rk3588/`

- `rk3588_bl31_v1.51.elf` — ARM Trusted Firmware BL31 (passed as `BL31=`).
- `rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin` — DDR init TPL (passed as
  `ROCKCHIP_TPL=`). Board-memory-specific, matching the RK1's LPDDR4/5 setup.

`boot2deb update` hashes these to fill the lock's `[blobs]` pins.

## Source and redistribution

The blobs come from Rockchip's `rkbin` repository
(<https://github.com/rockchip-linux/rkbin>, `bin/rk35`). Rockchip's license grants
a non-exclusive right to use, copy, and distribute the binaries and to distribute
modifications, provided the copyright, patent, and trademark notices are
preserved; the binaries are provided as-is. Vendoring them here is redistribution
under those terms. See `THIRD-PARTY-NOTICES.md` at the repository root for the full
attribution.
