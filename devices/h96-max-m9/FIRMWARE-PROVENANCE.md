# H96 MAX M9 — AIC8800D80 firmware provenance

The board's Wi-Fi firmware is **not vendored in this repository.** It is fetched at
build time from the same pinned upstream as the driver — `radxa-pkg/aic8800`, at the
commit the recipe's `.lock` records — and packaged into an `aic8800-firmware` deb by the
kmod build node (see the device's `[[device_kmods]]` block and its `firmware` mapping).
Driver and firmware move together on one pin, so the firmware always matches the exact
driver revision it was validated with.

**Licensing is UNSTATED.** AICSemi ships no explicit firmware license, and radxa
redistributes the blobs without a stated grant. boot2deb therefore hosts none of these
bytes: they are pulled from radxa's public repository at a pinned commit, not stored
here or in `boot2deb-blobs`. **Their redistribution terms must be reviewed before any
image built with this firmware is published.**

## Source and install

- Source: `radxa-pkg/aic8800`, `src/SDIO/driver_fw/fw/aic8800D80/` at the locked commit.
  radxa's D80 set is byte-different from a factory-eMMC extraction; it is the set paired
  with radxa's driver revision, which is what boot2deb builds.
- Install path: `/usr/lib/firmware/aic8800_fw/SDIO/aic8800D80/` — the path
  `fix-sdio-firmware-path.patch` compiles into the BSP loader (`CONFIG_AIC_FW_PATH`).
- Packaging: `aic8800-firmware`, `Architecture: all`, no kernel dependency. Kept out of
  the per-kernel `aic8800-modules-<kver>` deb so two coexisting kernels (A/B slots, an
  in-progress upgrade) never own the same firmware path and collide in dpkg.

## Files the driver loads

From the hardware bring-up, the D80 SDIO driver requests: `fw_patch_table`, `fw_adid`,
`fw_patch`, `fw_patch_ext0`, `fmacfw_8800d80_u02`, `lmacfw_rf`, and `aic_userconfig`.
The whole `aic8800D80/` directory is staged, so the exact request set is covered without
tracking each filename here.

## Bluetooth

Out of scope. The combined Wi-Fi+BT firmware (`fmacfwbt_*`) is present in radxa's set but
not enabled: the board's BT is a separate UART path, and the SDIO-to-BlueZ transport is
unverified. Enabling it would require dropping the driver's `BROKEN` gate and validating
the HCI path out of tree.
