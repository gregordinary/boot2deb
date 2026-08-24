# The maskrom loader

Some boards' u-boot builds emit a **maskrom loader** — `u-boot-rockchip-maskrom.bin`, a
single file you stream to a Rockchip SoC over USB to run that u-boot from RAM, writing
nothing to storage.

It exists for the case where the board's own bootloader cannot help you. A Rockchip SoC
whose eMMC carries no bootloader, or a broken one, still enters the BootROM's download
mode, and the BootROM will accept a bootloader over USB and jump to it. That is the floor
beneath every other recovery path: it does not need the board to boot, and it does not need
the case open.

## When you get one

The artifact appears when the board's u-boot build enables
`CONFIG_ROCKCHIP_MASKROM_IMAGE`. On a build that produces it, three files are staged
beside the usual `idbloader.img` and `u-boot.itb`:

| Artifact | What it is |
|---|---|
| `u-boot-rockchip-usb471.bin` | CODE471 — the DDR init blob (TPL), which the BootROM runs first to bring RAM up |
| `u-boot-rockchip-usb472.bin` | CODE472 — SPL plus the FIT, which runs once there is RAM to run it in |
| `u-boot-rockchip-maskrom.bin` | the two above packed into one RKBOOT container |

The split matters because the two host tools want different things. Tools that speak the
download protocol directly take the raw pair, in order. `rkdeveloptool db` takes the single
packed container. Both are staged so you do not have to convert between them.

```sh
boot2deb build <recipe> --stage uboot
```

## Using it

Put the board in maskrom mode — how differs per board; see that board's page — and confirm
the host sees it:

```sh
lsusb | grep 2207          # 2207 is Rockchip's vendor ID
rkdeveloptool ld           # lists devices and their mode
```

Then stream the loader and let it run:

```sh
rkdeveloptool db u-boot-rockchip-maskrom.bin
```

The board now runs that u-boot out of RAM. Nothing has been written, so a power cycle
returns it to exactly the state it was in — which is what makes this safe to try on a
board you have not diagnosed yet.

## Why boot2deb packs it itself

Rockchip's own `boot_merger` builds this container, but it ships as a closed binary in the
vendor `rkbin` repository. boot2deb writes the format directly instead, in pure Rust, so
producing a loader needs no vendor tooling and the output is deterministic — the same
inputs give the same bytes, which is what lets the artifact cache treat it like any other
build output.

The container holds only the two download sections. A full vendor loader also carries a
`LOADER` section — the blobs a host writes *to storage* — which `db` never downloads, and
which is the only part bearing a signed or encrypted header. Omitting it is what keeps the
writer pure: there is nothing to sign, so there is nothing that needs a vendor key.
