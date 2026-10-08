# vboot test fixtures

The fixtures `image::vboot` is tested against. Each signed partition is real `futility`
output, so the tests hold the in-process signer to the reference implementation byte for
byte rather than to itself.

| file | what it is |
| --- | --- |
| `kernel_data_key.vbprivk` | the vboot developer kernel data key (algorithm 4, RSA-2048/SHA-256): the key every boot2deb depthcharge image signs its kernel with, from `/usr/share/vboot/devkeys` |
| `kernel_subkey.vbprivk` | the vboot developer kernel subkey (algorithm 7, RSA-4096/SHA-256), from the same directory: a valid key that did not sign either partition |
| `signed.kpart` | a 6000-byte stand-in kernel packed for `arm` with the data key, at the default 64 KiB header padding `depthchargectl` also uses |
| `resigned.kpart` | `signed.kpart` repacked with only its command line changed |

The command lines are:

```
signed.kpart    kern_guid=%U console=tty1 rootwait ro loglevel=4 root=PARTUUID=0a6f3e5c-2b1d-4c8e-9f70-1a2b3c4d5e6f
resigned.kpart  kern_guid=%U console=tty1 rootwait ro loglevel=4 root=PARTUUID=7d2c9b4e-5f61-4a83-b0c2-d3e4f5a6b7c8
```

The stand-in kernel is the byte sequence `(i * 7 + 3) mod 256` for `i` in `0..6000`, and
the bootloader stub is 512 zero bytes. With the developer keys in `$K`, each command line
written to a file with no trailing newline, the partitions are:

```sh
futility vbutil_kernel --pack signed.kpart --keyblock $K/kernel.keyblock \
    --signprivate $K/kernel_data_key.vbprivk --version 1 --vmlinuz vmlinuz.bin \
    --config cmdline.txt --bootloader bootloader.bin --arch arm
futility vbutil_kernel --repack resigned.kpart --oldblob signed.kpart \
    --config cmdline-resigned.txt --signprivate $K/kernel_data_key.vbprivk \
    --keyblock $K/kernel.keyblock
```

The keys are test material published with vboot_reference, copyright The Chromium OS
Authors, under the BSD-3-Clause license. Debian ships them in `vboot-kernel-utils`.
