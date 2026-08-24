# The on-image self-test

Every image carries `boot2deb-selftest`, a small POSIX-sh program that compares
what the image claims to be against what is actually running, and exits non-zero
on any disagreement. It exists for one class of failure: the board that boots,
logs in, and is quietly missing something — a GPU whose firmware file moved, a
`/boot` with no initrd, a sound card whose codec never probed, a PREEMPT_RT
kernel nobody asked for. None of these announces itself; each has cost a
debugging session that started from the symptom instead of the cause.

```sh
# On the board. Root is needed to read dmesg and the initramfs.
sudo boot2deb-selftest
```

```
turing-rk1 / forky / rk3588-mainline-7.2

identity
  ok      kernel-release    7.2.0
  ok      kernel-flavor     arm64
  ok      dtb               rockchip/rk3588-turing-rk1.dtb
  ok      single-kernel     7.2.0-1-arm64
soc-rk3588
  ok      firmware          arm/mali/arch10.8/mali_csffw.bin
  FAILED  driver-bound      fb000000.gpu panthor  (no /sys/bus/*/drivers/panthor/fb000000.gpu)
  ...

12 ok, 1 failed. Failures are what this image was expected to have and does not.
```

The exit code makes it composable: 0 when nothing failed, 1 on any failure, 2
when no checks are installed at all. It can be the last thing a bring-up session
runs, a step in a serial-console validation script, or a once-per-boot journal
entry (see [Running it every boot](#running-it-every-boot)).

## Where the checks come from

The checks are not written on the device and the runner parses no TOML. Each
config layer may declare an `[[expect]]` array — the SoC, the boot method, the
device, the kernel definition, a feature, a kmod — and the build flattens each
layer's entries into its own file under `/etc/boot2deb/selftest.d/`:

```
identity.checks                 derived from the build itself (see below)
soc-rk3588.checks               socs/rk3588.toml [[expect]]
boot-method-rockchip-rkbin.checks
device-turing-rk1.checks
kernel-rk3588-mainline-7.2.checks
feature-media-accel-rockchip.checks
kmod-aic8800.checks
```

One file per layer is deliberate: a failing check names the layer whose
contract it is, which is where the fix — or the stale expectation — lives. Two
layers declaring the same check run it twice; each file states its own layer's
contract, and de-duplicating across them would let one layer's edit silently
change another's file.

`identity.checks` is generated, never authored. It restates what the lock and
the resolved device already own — the pinned kernel version, the `uname -r`
flavor suffix, the board's DTB — because a layer restating any of those could
drift from the pin. It also carries `single-kernel`, which restates nothing and
belongs to no layer: it is a claim about how every boot2deb image is built, and
only the build is in a position to make it.

Each line is one check: the kind, then its argument text to the end of the
line. Blank lines and full-line `#` comments are skipped. There is no quoting
and no escaping; the build validates at config load that no argument needs any.

## The check kinds

| Kind | Passes when | Example |
| --- | --- | --- |
| `file` | the absolute path (globs allowed) matches something | `file /boot/initrd.img-*` |
| `dtb` | the blob is installed in any layout the shipped kernels use | `dtb rockchip/rk3588-turing-rk1.dtb` |
| `firmware` | the path exists under `/lib/firmware` (compressed spellings included) | `firmware arm/mali/arch10.8/mali_csffw.bin` |
| `initramfs-module` | the module is built into the installed kernel **or** present in its initramfs | `initramfs-module dw_mmc-rockchip` |
| `driver-bound` | `/sys/bus/*/drivers/<driver>/<device>` exists | `driver-bound fb000000.gpu panthor` |
| `devnode` | the node exists under `/dev` (globs allowed) | `devnode /dev/dri/renderD128` |
| `sound-card` | the name appears in `/proc/asound/cards` | `sound-card H96 Analog` |
| `no-dmesg-match` | the POSIX ERE does **not** match the kernel log | `no-dmesg-match SError\|Synchronous External Abort` |
| `kernel-release` | `uname -r` starts with the pinned version (generated only) | `kernel-release 7.2.0` |
| `kernel-flavor` | `uname -r` ends in the flavor and is not its `-rt-` variant (generated only) | `kernel-flavor arm64` |
| `single-kernel` | `/boot` holds exactly one kernel and its module tree is that kernel's (generated only; takes no argument) | `single-kernel` |

A check kind the runner does not know is reported `skipped`, never failed — an
image built by an older boot2deb than the config tree that later grew a new
kind must not fail for it. Skips are loud in the output for the same reason a
silent skip is banned everywhere else: a check that quietly stops running looks
exactly like a check that passes.

`kernel-flavor` earns its two lines of logic: `7.2.0-1-rt-arm64` also ends in
`-arm64`, so the runner refuses the `-rt-` spelling first. That is the check
that catches an accidentally-RT kernel, which boots fine and quietly changes
scheduling behaviour.

`single-kernel` is the one check with no argument, because there is nothing to
parameterize: an image installs one solved package plan with one `linux-image`
in it and never dist-upgrades mid-build, so a second version on `/boot` means a
feature or a `--deb` addition pulled one in. It checks the module tree too — one
kernel whose `/usr/lib/modules` is for a different version is the shape a
half-swapped kernel leaves, and a bare count would call that healthy. A failure
names the versions it found and stops there: losing a kernel is worse than
shipping two, so nothing is swept.

It is an **as-built** invariant, and that is worth knowing before you meet it on
a long-lived board. A system that has since installed a second kernel — a
`distro-package` build that took an `apt` kernel upgrade, which keeps the old
one on purpose — genuinely differs from the image it was flashed from, and the
check says so. That is the report doing its job rather than a false alarm; where
you want two kernels on purpose, delete the line from `identity.checks`.

## Authoring an expectation

When a board teaches you something — a firmware path its driver demands, a
device node that proves a subsystem came up, a dmesg signature of a failure you
never want to meet twice — write it down where the knowledge belongs, as the
thing that failed would have been caught:

```toml
# In the layer that owns the fact: socs/<soc>.toml, devices/<name>.toml,
# kernels/<id>.toml, features/<name>.toml, kmods/<name>.toml, or a
# boot-methods/*.toml.
[[expect]]
check  = "driver-bound"
device = "fb000000.gpu"
driver = "panthor"
```

Placement follows the same rule as caveats: the SoC layer for what every board
on the part has, the device for what one board wires up, the kernel definition
for what its patches and fragments deliver, a feature for the capability's own
proof, a kmod for the driver's runtime contract. Two placements deserve
calling out:

- **Firmware for a blob-loading driver goes on the kernel definitions that can
  load it**, not the SoC. A `libre` kernel drops non-free firmware by design,
  so a SoC-wide blob check would fail a correctly built libre image.
- **Boot artifacts go on the boot method.** `/boot/extlinux/extlinux.conf`
  exists on a `rockchip-rkbin` board and never on a depthcharge one, whose
  kernel lives in a signed GPT partition no file check can see.

An unknown kind, a missing argument, or an argument that belongs to a different
kind fails at config load, naming the field — a typo is caught by `resolve` (or
any other command), not on the board. `no-dmesg-match` patterns deserve care in
the other direction: author them narrowly, because a pattern that matches a
benign line makes every run red and teaches people to ignore the tool.

The caveat rule in the [config model](config-model.md) is the flip side of this
page: a limitation that is mechanically checkable belongs here, where it fails,
and only what cannot be checked from the running system belongs in a caveat.

## Running it every boot

```toml
# In the recipe: run the selftest once per boot, after multi-user.target,
# logging to the journal. Off by default.
selftest_on_boot = true
```

Every image ships the `boot2deb-selftest.service` unit disabled; the flag adds
the enable symlink. It is meant for boards validated over a serial console,
where nobody is logged in to run the check by hand — the failed unit in
`systemctl --failed` and the journal entry are the announcement:

```sh
# Read the last boot's result.
journalctl -u boot2deb-selftest -b
```

The unit carries `ConditionVirtualization=no`: the hardware checks describe the
board, and under emulation the `boot2deb try` harness runs the selftest itself
in the mode built for that.

## Running it against a node you are not sitting at

Over the network, it is one line — the check runs on the board, where the
hardware half of it means something:

```sh
ssh operator@rk1-03 'sudo boot2deb-selftest'
```

That is the form to reach for in a validation script across a cluster: the exit
code is the result, and the output names the layer behind any failure.

A node with no network yet — one being brought up, or one whose networking is
exactly what you are checking — has only its serial console, and that is a
person at a console or a tool that drives one. boot2deb does not drive board
consoles: it produces images and knows what they should contain, and reaching
out to operate hardware is the same boundary that keeps it from writing devices.
What it gives that tooling is this runner and its exit code, already on the
image; `selftest_on_boot = true` above is the other half, since a node that
checks itself every boot needs nothing driven at all.

## Inspecting an image from outside

The runner takes `--root` so a mounted image can be checked without booting it,
and `--mode userland` so the hardware checks report `n/a` instead of failing on
a machine that is not the board:

```sh
# A mounted (or extracted) rootfs on any machine: check disk content only.
boot2deb-selftest --root /mnt/image --mode userland
```

In userland mode the kernel checks read `/boot` instead of `uname -r` — the
running kernel is whatever machine or emulator this is, and the question
becomes "does the image carry the kernel it pins", which is still answerable.
This is exactly how `boot2deb try` runs the selftest inside a QEMU-booted
guest, where the running kernel is a fixture and the board hardware is absent.
