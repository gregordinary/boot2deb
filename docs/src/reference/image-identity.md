# Image identity

Every image boot2deb builds carries **`/etc/boot2deb/image.toml`** — a small TOML
document in which the image says what it is.

It exists for readers that are not the running system. A tool repairing a board that will
not boot is looking at the disk from somewhere else: from a USB stick, or from a laptop
with the eMMC dumped to a file, quite possibly without mounting the filesystem at all.
Such a reader can work out a great deal from the disk itself — the partition table, the
boot scheme, the kernel in the signed slot. This file is for the part it cannot.

## What it looks like

A depthcharge board:

```toml
version = 1

[image]
device = "asus-c201"
description = "ASUS Chromebook C201 (RK3288, google,veyron-speedy)"
arch = "armv7"
soc = "rk3288"
boot_method = "depthcharge"
board = "speedy"
suite = "forky"
features = []
layout = "combined"
hostname = "asus-c201"

[kernel]
id = "debian-armmp"
flavor = "distro-package"
package = "linux-image-armmp"
```

A `rockchip-rkbin` board with a compiled kernel — no `board`, and the kernel is a git pin
rather than a package:

```toml
[image]
device = "turing-rk1"
boot_method = "rockchip-rkbin"
...

[kernel]
id = "rk3588-mainline-7.2"
flavor = "mainline"
reference = "v7.2"
commit = "8d3ae59288f1e7d58d76558a6ee96d533bc5019f"
patch_series = "rk3588-accel"
```

## `board` is the reason the file exists

Everything else here is a **cross-check**: a reader can already infer the device, the boot
method, and the architecture from the disk, and comparing what it inferred against what
the image claims is worth doing — a disagreement is itself a finding.

`board` is different. It is the depthcharge board profile the kernel partition was signed
for, and it is **not recoverable from the image**. `depthchargectl` normally works it out
by reading the *running* board's hardware ID and device-tree compatibles, which is exactly
what a tool running somewhere else cannot do. Re-signing a C201's kernel from a laptop
means passing `--board speedy`, and this file is how that laptop knows to.

It also distinguishes firmware, not just hardware: a stock C201 and a libreboot'd one are
the same board and take different profiles — which is why they are separate build points
(`asus-c201/forky` and `asus-c201-libreboot/forky`) rather than one image with a flag.

The field is absent under a boot method that has no board profile, rather than being an
empty string a reader would have to special-case.

## `kernel.flavor` decides how a kernel upgrade arrives

`distro-package` means the kernel comes from the Debian mirror and an upgrade is
`apt upgrade`. `mainline` or `vendor` means boot2deb compiled it, nothing will ever offer
it to the board, and a new one is a `.deb` somebody has to hand it. A tool that intends to
put a kernel on this system needs to know which.

## `layout` matters on a split image

Under `layout = "split"` the boot payload and the root filesystem live on **different
media** — u-boot on the eMMC, the OS on NVMe. A reader that finds this rootfs with no
bootloader beside it is looking at an expected state, not a fault.

## `pressed` marks a derived image

An image `press` re-assembled with tree additions carries one extra table:

```toml
[pressed]
source = "turing-rk1-forky"
copies = ["/etc/myapp/site.conf"]
debs = ["myapp_1.2_arm64.deb"]
embedded_image = "turing-rk1-forky.img.xz"
```

Its **absence is the claim**: no `[pressed]` table means this filesystem is the
recipe's canonical artifact, byte for byte. Where it is present, `source` names
the artifact stem the file derives from and the remaining keys list what was
added, by kind and destination — never by content. `reproduce` reproduces
builds, not pressings. The seed partition is deliberately not summarized here:
it is self-describing, and `boot2deb seed` can rewrite it later without
touching the filesystem, so a copy of it in this file would go stale. See
[Producing images](../press.md).

## It carries no secrets

boot2deb also emits a **provenance manifest** (a `.provenance.toml`) beside the
image, which records every source pin, the toolchain, the solved package manifest's
digest, and the image's initial first-boot password. That document stays with the build.
It never ships inside an image, and `image.toml` is a deliberately chosen subset of it
with the credential — and everything else an image has no business carrying — left out.

Two values that are in the manifest cannot be in `image.toml` even in principle: the
solved-manifest digest and the package count are *produced by* the rootfs bootstrap, so
they are not yet known at the moment the file is written into the rootfs they would
describe.

## Compatibility

`version` is the schema version, and this is a wire format: it is parsed by programs
versioned independently of boot2deb. A reader must check it, and must tolerate fields it
does not recognise. Adding an optional field does not bump the version; changing what a
field means, or removing one, does.

The file is written as part of the generated config, alongside `/etc/boot2deb/board.conf`,
so it folds into the rootfs cache key like every other generated file — a cached rootfs
can never be reused under an identity that disagrees with it.
