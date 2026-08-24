# What the base image contains

Every boot2deb image is the same substrate plus what its layers add. This page is the
substrate: what is on a board before any SoC, device, or feature package stacks on top,
and why each thing is there.

## How the set is built

Three inputs, in order:

1. **Debian's `required` + `important` priority set.** This is the base system — a
   working Debian userland with `apt`, `systemd`, `bash`, `less`, `nano`, `vim-tiny`,
   `procps`, `fdisk`, `iputils-ping`, and `tzdata`.
2. **The `packages` list in `base.toml`**, below.
3. **The layers**: `arches` → `socs` → `boot-methods` → `devices`, then any features the
   recipe names. Wi-Fi tooling, Mesa, `bluez`, and the media stack all arrive this way,
   because they are claims about hardware rather than about Debian.

Two properties of step 1 decide most of step 2, and they are worth stating plainly
because they differ from what `apt install` does on a desktop:

- **`Recommends` are never installed.** Resolution follows `Depends` only.
- **Priority `standard` is not part of the base set.**

So anything an ordinary Debian install would have reached by either route is absent
unless `base.toml` names it. `ca-certificates` is the case that shows why this matters:
it is priority `standard`, and nothing `Depends` on it — libcurl merely *Recommends* it —
so without an explicit entry an image would ship `curl` and `wget` that cannot fetch an
https URL.

## What `base.toml` adds

Sizes are installed size on forky/arm64, including each package's own dependency
closure where that closure is the interesting part.

| packages | why |
|---|---|
| `initramfs-tools`, `dbus`, `dhcpcd`, `libpam-systemd`, `systemd-timesyncd`, `sudo` | boot, session, clock, privilege. A board with no RTC gets its time from the network. |
| `openssh-server`, `openssh-client` | Remote access both ways. The server's `Depends` already pull `openssh-sftp-server`, so `scp`/`sftp` *to* a board work without the client; the client is what provides `scp`, `sftp`, `ssh-keygen`, and `ssh-copy-id` *from* it. |
| `ca-certificates`, `curl`, `wget`, `rsync` | Fetching and moving files, https included. |
| `bind9-dnsutils` (~6 MiB with `bind9-libs`) | `dig` and `host`, for triaging a board whose network is the thing that is wrong. |
| `unzip`, `zip`, `xz-utils`, `zstd` | Archives. |
| `pciutils`, `usbutils` | Hardware inventory. `lsusb` earns its place ahead of `lspci` on these boards: an SBC's peripherals are far more often on USB than on PCIe. |
| `htop`, `lsof`, `psmisc`, `xxd`, `file` (~10.6 MiB) | Looking at a running system, and at bytes. Nearly all of `file`'s cost is `libmagic-mgc`, the compiled magic database. |
| `bash-completion`, `man-db` (~9.2 MiB) | Shell usability. `man-db` pulls `groff-base`, `bsdextrautils`, and `libpipeline1`; it buys the ~3.9 MiB of manual pages every *other* package already puts on the image and that nothing else can read. |
| `locales`, `keyboard-configuration`, `console-setup` (~44 MiB) | What makes a **pre-built** image reconfigurable with no network. See [Locale, timezone, and keyboard](../localization.md). |

`isc-dhcp-client` is excluded; `dhcpcd` is the DHCP client, and boards whose SoC layer
brings NetworkManager exclude `dhcpcd` in turn.

## What is deliberately absent

- **`locales-all`** — 231 MiB installed. The 17 generated locales cost 19.2 MiB instead.
- **A desktop, a display manager, or a browser.** Images are headless by default; a
  desktop is `apt install` away, and the image ships the locale data one needs to open
  on something other than English.
- **`avahi-daemon` / mDNS.** It would open a listener on the LAN, which is not a default
  an appliance image should make for its owner.
- **`unattended-upgrades`.** An image that silently changes itself is not one whose
  provenance record still describes it.
- **Storage and hardware tooling with no hardware to point at** — `smartmontools`,
  `nvme-cli`, `mtd-utils`. Board-specific tools belong on the board's layer, as
  `i2c-tools` and `ir-keytable` are.
- **Development toolchains.** Nothing on an image builds software; that happens in the
  builder's sandbox.

## Adding to it yourself

There is no `--package` flag, for the same reason there is no build-time locale flag: an
image's contents come from the config its lock was resolved against, so a package that
is on the image is a package the config named and the manifest can pin.

The question is which layer the claim belongs to — `base.toml` for anything true of every
Debian image, the SoC or device layer for anything true of the hardware, and a feature
for anything a recipe should be able to opt into. To add packages without editing the
shipped tree at all, put your own layer in an overlay directory and pass `--overlay`; a
same-named layer is deep-merged over the shipped one. See [Config model](config-model.md)
and [Overlays](overlays.md).

On a running board it is just Debian: `sudo apt install tmux`.
