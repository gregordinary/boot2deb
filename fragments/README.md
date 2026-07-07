# fragments/ — kernel `.config` fragments

Kernel configuration is a base in-tree defconfig plus merged fragments, not a
full checked-in `.config`. Each kernel definition names
the version-coupled fragments it merges (`config_fragments`), and the device
layer appends its board fragment (`device_config_fragments`). The engine merges
them onto `make <base_defconfig>` with the tree's own
`scripts/kconfig/merge_config.sh` (last-wins, then `olddefconfig` to
dependency-resolve).

## Layout

| Fragment | Kind | Holds |
|----------|------|-------|
| `base/debian-arm64` | generated | The Debian-generic modular arm64 kernel policy — the bulk delta from `defconfig` to the target RK1 config (filesystems, netfilter, media, USB, sound, …). Not rockchip engineering; not hand-edited. |
| `soc/rk3588` | curated | RK3588 platform drivers: clk / pinctrl / phy / pcie / thermal / saradc / crypto / nvmem / regulator / sound-soc / display. Shared by every RK3588 board. |
| `accel/full` | curated | RK3588 media + compute accel: VDPU decode, VEPU encode, RGA multicore (OOT), rocket NPU, Hantro, ISP, VSI IOMMU. These symbols exist only after the `rk3588-accel` patch series is applied. |
| `device/turing-rk1` | curated | Board-only kconfig. Empty for the RK1 — its board delta is in the device tree, not kconfig. |

Merge order is base → soc → accel → device (device last so a board can override).

## Why a generated baseline plus curated slices

The target RK1 config differs from `defconfig` in ~6600 symbols, of which only
~60 are rockchip SoC/accel choices; the rest is generic Debian kernel policy.
Splitting that bulk "by concern" adds no value (it is Debian's policy, not ours),
so it lives in one generated baseline while the small rockchip slices stay
legible and reusable across kernels/SoCs. **The resulting `.config` depends only on
the union of the fragments**, so which slice a symbol lands in is a legibility
choice with no effect on the output.

Toolchain-probed symbols (MTE / RELR / `CC_*` / `AS_*` / `PAHOLE_*`) are **not**
checked into any fragment: they vary with the build host's assembler/compiler, so
each host derives them itself.

## Checking against a reference config

`boot2deb verify-config <recipe> --kernel-path <patched tree> --reference-config
<config>` regenerates the `.config` from these fragments and asserts it matches the
reference over the normalized `CONFIG_*` set. It compares against
`olddefconfig(<reference>)` — the config that tree + toolchain actually compiles —
so probed symbols cancel and only fragment differences surface.

## Regenerating on a kernel bump

A new kernel version is a re-validation event. The baseline is mechanical:
diff the new kernel's `make defconfig` against the target config, subtract the
curated slices and the toolchain-probed symbols, and take the fixpoint under
`olddefconfig` (so `default y` children disabled upstream get explicit pins). The
curated `soc/`, `accel/`, and `device/` slices are then reviewed by hand against
the kernel's config-drift report — that human curation is the point of the split.
