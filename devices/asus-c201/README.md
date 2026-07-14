# ASUS Chromebook C201 — vendored firmware

The device layer's `overlay/` tree carries two Broadcom firmware blobs, both vendored
because Debian ships neither, both landing in the image at
`/usr/lib/firmware/brcm/`.

| file | bytes | sha256 | role |
|---|---|---|---|
| `brcmfmac4354-sdio.txt` | 2907 | `1ed835efb2aa2f295aef30f00c01a044579e5b1d1fbf65f04e95add6a146f666` | Wi-Fi board NVRAM / calibration |
| `BCM4354.hcd` | 81417 | `22cd1a7ba3b7872cb368eab61cc4640b6638c3c3e0b25277a3cc2803a3a1de45` | Bluetooth patchram |

## Why these are vendored and the Wi-Fi `.bin` is not

Debian's `firmware-brcm80211` (pulled in by `socs/rk3288.toml`) carries the
`brcmfmac4354-sdio.bin` and its `clm_blob`, so those come from the mirror like any
other package. It carries neither of the files above:

- The only BCM4354 NVRAM in Debian is `brcm/brcmfmac4354-sdio.nvidia,p2371-2180.txt`
  — an nVidia Jetson TX1 board file. NVRAM is not tuning; `boardtype`, `boardrev`, and
  `devid` are the module's *identity*, and `brcmfmac` will not initialise an SDIO
  device without the right one.
- Debian ships **no BCM4354 `.hcd` at all**. Veyron Bluetooth is UART/serdev
  (`brcm,bcm43540-bt`), so `hci_bcm` asks for a chip-named patchram file with no
  USB VID:PID to key on, and Debian's set is keyed the other way.

## Provenance

Taken from [`jenneron/firmware-google-veyron-brcm`](https://github.com/jenneron/firmware-google-veyron-brcm),
which is what postmarketOS installs on these boards. These bytes are the ones running
on the reference C201 today, confirmed by hash against the unit's own installed copy.

The board is a **BCM94354Z NGFF/M.2** module: `boardtype=0x0707`, `boardrev=0x1224`,
`devid=0x43a3`.

**Do not substitute ChromiumOS's `linux-firmware` copy of `brcmfmac4354-sdio.txt`.** It
describes a *different module* — a BCM94354 WLBGA sample, `boardtype=0x0703`,
`devid=0x43df` — and `0x43df` is the PCIe BCM4354's device ID, so it is keyed to a
variant this board does not have. Wrong module, wrong calibration.

## Why the generic filenames

`brcmfmac` and `hci_bcm` try a DT-compatible-suffixed name first
(`brcmfmac4354-sdio.google,veyron-speedy-rev9.txt`) and fall back to the generic one.
Installing under the generic name therefore serves every Veyron board revision and
every Broadcom board in the family (speedy, minnie, mickey, brain) from one copy.

Installing them anywhere *else* is what breaks Wi-Fi on the unit's postmarketOS
install, which puts them under `/lib/firmware/postmarketos/brcm/` with no
`firmware_class.path` set — the driver looks in `/lib/firmware/brcm/` and gets `-2`.

## Licence

Redistributable Broadcom firmware, the same class as the blobs in Debian's
`firmware-brcm80211` (`non-free-firmware`). Not covered by this repository's licence;
see `THIRD-PARTY-NOTICES`.
