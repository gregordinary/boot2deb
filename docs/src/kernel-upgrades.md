# Upgrading the kernel

On a ChromeOS-firmware board — the [C201](boards/asus-c201.md), the
[C100P](boards/asus-c100p.md), the [Chromebit](boards/asus-chromebit-cs10.md), and every
other board using the `depthcharge` boot method — upgrading the kernel is `apt upgrade`,
and it is **atomic and reversible**. If a new kernel does not boot, the firmware puts the
old one back on its own. You do not have to do anything, and you do not need a USB stick.

That is worth stating plainly, because on these boards it is not obvious. The kernel is
not a file in `/boot` that a bootloader reads. It is a **vboot-signed blob written raw
into a partition**, and changing it means re-signing it and rewriting that partition —
which is exactly the operation you cannot afford to get wrong, because it is the only
thing standing between the board and a firmware screen.

## How it works

The image ships **two ChromeOS kernel slots**, `KERN-A` and `KERN-B`. The kernel lives in
one of them; the other is empty. Both are ordinary GPT partitions, and the firmware picks
between them using three fields in each partition's GPT attribute bits:

| field | meaning |
|---|---|
| `priority` | boot order among candidates; `0` means **never boot** |
| `tries` | attempts remaining, decremented *before* each attempt |
| `successful` | known-good; stops the firmware spending `tries` |

A slot is a boot candidate while `priority > 0` and (`successful` or `tries > 0`).

When a kernel package is installed — by `apt`, or by `dpkg -i` on a `.deb` you built
yourself — Debian's `depthcharge-tools` package runs its `/etc/kernel/postinst.d` hook,
which:

1. rebuilds the kernel FIT (kernel + device tree + initramfs) and signs it,
2. writes it into the slot the board is **not currently booted from**,
3. marks that slot highest-priority with `tries = 1, successful = 0`.

On the next boot the firmware spends that single try on the new slot. If the system comes
up, `depthcharge-tools.service` runs `depthchargectl bless`, which sets `successful` and
commits the upgrade. If the system *never* comes up, the try is already spent, the slot
stops being a candidate, and the firmware falls back to the other slot — which still holds
the kernel that worked, still marked `successful`.

So a failed kernel upgrade costs you one reboot. Nothing else.

> **The spare slot is the whole mechanism.** An image with only one kernel slot cannot do
> any of this: the only slot is the one you are running from, so an upgrade has to
> overwrite the running kernel in place, and a kernel that does not come up leaves the
> board with nothing to boot and no way in but external media.

## Doing it

Nothing special:

```sh
sudo apt update && sudo apt upgrade
```

If the upgrade includes a kernel you will see `depthchargectl` run in the output. Reboot
when it finishes. That is the entire procedure.

## Checking which slot you are on

```sh
sudo depthchargectl list
```

That prints every ChromeOS kernel partition on the disk with its size and its `S`/`P`/`T`
(successful / priority / tries) attributes. A healthy board after a committed upgrade
looks like two slots, both `S=1`, the running one at the higher priority.

The board also tells the running system which slot it booted from: the firmware
substitutes that partition's PARTUUID into `kern_guid=` on the kernel command line.

```sh
grep -o 'kern_guid=[^ ]*' /proc/cmdline
```

That value is what `depthchargectl` uses to know which slot it must *not* overwrite.

## Rolling back on purpose

If a kernel boots but is bad in some way you only notice later, mark it unbootable and
reboot — the firmware falls back to the other slot:

```sh
sudo depthchargectl bless --bad      # zero the running slot's attributes
sudo reboot
```

Then pin or downgrade the kernel package so the next `apt upgrade` does not simply put it
back.

## When the write fails: the payload ceiling

The signed blob must fit its kernel slot — 16 MiB on stock firmware (a board page may
list roomier firmware profiles). `depthchargectl` builds the new image first and writes
second, so when nothing it tries fits under the ceiling, it fails **without touching the
slot**:

```
Couldn't build a small enough image for this board
```

Nothing is broken when this prints. The slot still holds the payload the board booted
from and the board keeps booting — but the change that triggered the rebuild has not
reached the slot, and nothing will until the payload fits again.

The payload is kernel + device tree + initramfs, and the part that grows is the
initramfs. The image ships it deliberately small — an explicit module list
(`MODULES=list`) and xz compression — which leaves about 2 MB of headroom under the
stock ceiling.

**What spends that headroom is initramfs-tools hooks**, and the one that spends it all at
once is plymouth. Desktop metapackages (`cinnamon-desktop-environment`,
`task-gnome-desktop`, and the rest) pull plymouth in through Recommends, `desktop-base`
registers a graphical boot theme, and plymouth's hook then copies the splash daemon, its
renderers, the theme, and the text plugin with its whole font stack into every initramfs
built afterwards. That is several MB, and the next slot write — a kernel upgrade, or any
package that triggers `update-initramfs` — fails as above.

The cost buys nothing on these boards. The initramfs carries no DRM modules, so plymouth
cannot draw before the root pivot regardless — it says so itself during the rebuild:

```
W: plymouth: not including drm modules since MODULES=list
```

Remove it:

```sh
sudo apt purge 'plymouth*'
```

The purge re-triggers the initramfs rebuild and the slot write; watch the
`depthchargectl` run in the output succeed. Nothing depends on plymouth — desktops only
recommend it — and no configuration keeps an installed plymouth out of the initramfs
(its initramfs-tools fragment overrides any admin setting), so removing the package is
the supported answer. If the original failure aborted an `apt` run partway, finish it
first with `sudo dpkg --configure -a`.

If the write still fails, something else grew the initramfs. List its contents by size
and look for what does not belong:

```sh
lsinitramfs -l /boot/initrd.img-$(uname -r) | sort -k5 -rn | head -20
```

A healthy initramfs for these boards is 7–8 MB compressed (`ls -lh /boot/initrd.img-*`).

## Does it differ with a compiled kernel?

**No — the mechanism is identical.** The hook that re-signs and writes a slot is triggered
by the *kernel package's own maintainer script*, not by `apt`, so it fires for any
`linux-image` `.deb` that gets configured, however it arrived. A boot2deb-compiled kernel
and Debian's stock `linux-image-armmp` take exactly the same path through the same tool,
and both get the same A/B safety.

What differs is **delivery**, and only that:

| | where the kernel comes from | how you upgrade |
|---|---|---|
| distro kernel (`debian-armmp`) | the Debian mirror | `apt upgrade` |
| compiled kernel | boot2deb's `--stage kernel` output | copy the `.deb` to the board, `dpkg -i` |

A compiled kernel is not on any mirror, so nothing will ever offer it to you — you deliver
the `.deb` yourself. Once `dpkg` configures it, the slot is written and the reboot is as
safe as any other.

> **The upgrade unit is the `.deb`, not the signed blob.** It is tempting to think of the
> signed kernel partition as the thing you ship, and it is not. The signed blob contains
> the kernel, the device tree, and the initramfs — but *not* the kernel modules, which
> live on the root filesystem in `/lib/modules/<version>` and are where Wi-Fi, graphics
> and sound actually come from. A kernel written without its modules boots into a system
> with no drivers. The `.deb` carries both, which is why it is what you move around.
>
> The signed blob is also **specific to the machine it was built on**: first boot gives
> each board its own root filesystem PARTUUID, and that value is baked into the
> *signature*. A blob signed for one board's PARTUUID cannot find root on another.

## The other boards

On a `rockchip-rkbin` board — the [Turing RK1](boards/turing-rk1.md), the H96 — none of
this applies. There the kernel is an ordinary file in `/boot`, the bootloader is u-boot
reading `extlinux.conf`, and a kernel upgrade rewrites that config file. It is simpler,
and it has no rollback: the boot configuration is a file, so a bad kernel is fixed by
editing it back, which needs a keyboard and a screen or a serial console.
