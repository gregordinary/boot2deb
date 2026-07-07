# Adding a board

Bringing up a new device means writing config layers, not code. A build resolves from
TOML across the [config model](../reference/config-model.md)'s axes, so a new board is a
set of small TOML files plus any vendored blobs and kernel fragments. The shipped
`turing-rk1` files are the worked reference — copy the closest existing layer and change
its deltas.

## The layers to write

Work from the bottom of the hardware stack up, adding only what is new:

1. **arch** (`arches/<arch>.toml`) — only if the CPU architecture is not already present.
   Holds arch-wide defaults (the cross triple, base kernel/deb arch).
2. **soc** (`socs/<soc>.toml`, plus `socs/<soc>/overlay/` for files baked into the
   rootfs) — the SoC's shared properties: device-tree directory, modules, arch.
3. **boot-method** (`boot-methods/<method>.toml`) — how this family boots: the u-boot ref
   and defconfig, and the **image offsets** (where `idbloader` and `u-boot.itb` sit in the
   raw gap, and where the rootfs partition starts). `boot-methods/<method>/overlay/` ships
   any boot-time files (e.g. the extlinux generator).
4. **device** (`devices/<device>.toml`) — the board itself, stating only its deltas: its
   `soc`, `boot_method`, `uboot_defconfig`, `kernel_dtb`, `image_size`, `hostname`, its
   `supported_kernels` / `default_kernel`, `default_suite`, `default_layout`, and any
   board-memory-specific bootloader blobs (e.g. the `[rkbin]` DDR TPL + ATF for a Rockchip
   board — these are board-specific, so they live here, not at the soc layer).
5. **kernel** (`kernels/<kernel>.toml`) — the orthogonal kernel axis: its source refs,
   `.config` fragments, and patch profile. Version-coupled, so a new kernel version is a
   new file.

Supporting assets:

- **blobs** (`blobs/<soc>/`) — vendored bootloader binaries the boot-method references.
- **fragments** (`fragments/<name>.config`) — kernel `.config` fragments merged onto the
  base defconfig. A device or kernel references these by name.
- **patch profile** — lives in the separate `patches` repo, referenced by the kernel.

Finally, a **recipe** (`recipes/<recipe>.toml`) pins one point across all the axes —
device, kernel, suite, profile, layout.

## Bring it up

Once the layers exist, use the CLI's checks as guardrails, in order:

```sh
# 1. Does it resolve to a coherent build point?
cargo run -p boot2deb-cli -- resolve <recipe>

# 2. Resolve upstream refs + hash blobs into the lock.
cargo run -p boot2deb-cli -- update <recipe> --kernel-ref <ref>

# 3. Is the host equipped to build it?
cargo run -p boot2deb-cli -- doctor <recipe>

# 4. Does the patch series apply cleanly to the pinned kernel?
cargo run -p boot2deb-cli -- verify-patches <recipe> \
  --kernel-path /path/to/linux --patches-path ../patches

# 5. Does the .config generate (and, if you have a reference, match it)?
cargo run -p boot2deb-cli -- verify-config <recipe> --kernel-path /path/to/patched-linux

# 6. Build.
cargo run -p boot2deb-cli -- build <recipe>
```

`resolve`, `update`, and the two `verify-*` commands fail with a typed error before any
compile starts, so most config mistakes surface in seconds rather than partway through a
build. When the image geometry is wrong (offsets overlap, or the rootfs will not fit), the
image node's up-front geometry check reports it before any stage runs.

## Flashing

Flashing is inherently per-board — a Turing Pi module flashes through the BMC, a standalone
SBC takes an SD card or a maskrom loader, a laptop boots UEFI. Document your board's path on
its own page under [Boards](../boards/turing-rk1.md), the way the Turing RK1 page does.
