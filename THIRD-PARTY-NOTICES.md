# Third-party notices

boot2deb vendors a few third-party components so that builds are reproducible and
work offline. Each is listed here with its origin and license. boot2deb's own
sources are under the project license stated in the repository root; the
components below retain the license of their upstream and keep their original
copyright notices.

## Bootloader firmware blobs — `blobs/rk3588/`

- `rk3588_bl31_v1.51.elf` — ARM Trusted Firmware BL31 for RK3588 (passed to the
  u-boot build as `BL31=`).
- `rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin` — DDR init TPL for RK3588
  (passed as `ROCKCHIP_TPL=`).

Source: Rockchip's `rkbin` repository, <https://github.com/rockchip-linux/rkbin>
(`bin/rk35`). Rockchip's license grants a non-exclusive right to use, copy, and
distribute the binaries and to distribute modifications, on the condition that
copyright, patent, and trademark notices are preserved; the binaries are provided
as-is with no warranty. Vendoring them here is redistribution under those terms.

## Boot and kernel-hook scripts — `boot-methods/rockchip-rkbin/overlay/`

- `boot/mk_extlinux`
- `etc/kernel/postinst.d/dtb_cp`
- `etc/kernel/postinst.d/kernel_chmod`
- `etc/kernel/postrm.d/dtb_rm`

Copyright (C) 2023 John Clark <inindev@gmail.com>. These descend from the
GPLv3-licensed `inindev/debian-nanopi-r6s` project and are licensed under the GNU
General Public License, version 3. Each file keeps its original copyright header.

## Debian archive keyring — `blobs/keyrings/debian-archive-keyring.gpg`

Extracted from Debian's `debian-archive-keyring` package, which is distributed
under the GNU General Public License, version 2. The exact package version, source
URL, and sha256 are recorded in
`blobs/keyrings/debian-archive-keyring.README.md`.

## Broadcom BCM4354 firmware — `socs/rk3288/overlay/usr/lib/firmware/brcm/`

- `brcmfmac4354-sdio.txt` — Wi-Fi board NVRAM / calibration data for the Veyron
  Chromebooks' BCM94354Z NGFF radio module.
- `BCM4354.hcd` — Bluetooth patchram for the same part.

Redistributable Broadcom firmware, the same class as the blobs Debian ships in
`firmware-brcm80211` (its `non-free-firmware` component). Broadcom's license permits
redistribution of the unmodified binary; the files are provided as-is with no warranty.

They are vendored because Debian ships neither: its only BCM4354 NVRAM is an nVidia
Jetson board file, and it carries no BCM4354 `.hcd` at all. Taken from
<https://github.com/jenneron/firmware-google-veyron-brcm>, which is what postmarketOS
installs on these boards. They sit on the SoC layer because they identify the radio
module, which is the same one on every Broadcom board in the family. Provenance,
hashes, and why the superficially similar ChromiumOS copies are the *wrong module* are
recorded in `socs/rk3288/README.md`.
