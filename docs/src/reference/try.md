# Trying an image before flashing

`boot2deb try` boots the built image under QEMU and asserts that the userland
works — before a card is written, a board is opened, or a serial console is
wired. It is the answer to the failure class that survives a clean build and a
clean flash: the journald that SIGILLs on its first message, the first-boot
hook that shipped without its executable bit and bricks the *second* boot,
the account whose generated password does not actually log in.

```sh
# Boot the built image twice under QEMU and assert the userland works.
boot2deb try turing-rk1/forky

# Keep the booted disk for a post-mortem, and allow a slower first boot.
boot2deb try turing-rk1/forky --keep-disk --timeout 1800
```

```
try turing-rk1/forky: PASS
  first boot   running, first-boot completed, selftest: 11 ok, 4 not applicable.
  second boot  running, first-boot did not re-run, selftest: 11 ok, 4 not applicable.
```

## What it asserts

One run is two boots of the same disk, and each boot must pass all of:

1. systemd settles as `running` — no unit in `systemctl --failed`.
2. The image's account logs in on the serial console with the generated
   first-boot password from the provenance manifest. The first login walks the
   forced password change the image ships with; that the whole conversation
   works *is* the account assertion, not a side effect of it.
3. `first-boot` ran to completion and wrote its stamp — and, on the second
   boot, **did not run again**. The second boot is the whole reason `try`
   exists as more than a smoke test: a brick-on-second-boot image passes every
   single-boot check ever written.
4. The [on-image self-test](self-test.md) passes in userland mode: the
   disk-content half of the board's expectations (kernel on `/boot`, firmware
   files, initramfs modules) checked inside the guest, with the hardware
   checks reported not-applicable rather than failed.

`try` exits non-zero on the first assertion that fails, with the guest's
console tail in the error.

## What it deliberately does not test

The **board**. The guest machine is `-M virt`: no RK3588, no panel, no codec —
the hardware half of the selftest runs on the device, not here. And the **boot
path**: QEMU loads the kernel directly, so u-boot, extlinux, and the
depthcharge signing are exercised on hardware only. `try` replaces a
flash-plus-serial-console cycle for userland faults; it replaces nothing else.

The shipped kernel is not booted either. It is configured from the board's
fragments and has no reason to carry virtio drivers — adding them would change
the shipped kernel to serve the test, which is backwards. The guest boots the
**suite's own generic kernel** (`linux-image-arm64` / `linux-image-armmp`),
fetched and installed through the same pinned, sandboxed machinery as every
package stage, its initramfs built by `initramfs-tools` inside a target-arch
root. The kernel is a fixture; the userland is what is under test. For a
`distro-package` kernel build the two coincide, and `try` says so. The pair is
cached under the recipe's work dir; `--refresh-fixture` re-harvests it when the
suite's kernel moves.

## Mechanics worth knowing

- The image artifact is never booted directly: the run decompresses a copy
  under `<work-dir>/try/` and boots that. `--keep-disk` keeps it afterwards —
  note the forced first-login change means the kept disk's password is no
  longer the generated one; the run's report prints the one now set.
- The run needs the build's artifacts *and* its provenance manifest (for the
  password), so it follows `boot2deb build` — the build's closing hint says so.
- `systemd-modules-load.service` is masked on the guest's kernel command line:
  the image force-loads board-kernel modules the fixture kernel cannot have,
  and that is a fact about this boot, not about the image.
- Runtime is minutes under TCG emulation on an x86 host — this replaces a
  flash-and-serial cycle, not a unit test. On a matching host with `/dev/kvm`
  (an arm64 box building arm64 images), KVM makes it fast.
- `qemu-system-aarch64` / `qemu-system-arm` is the one host tool involved, and
  it is optional: `boot2deb doctor <recipe>` lists it as such, with the package
  name for the host distro.
