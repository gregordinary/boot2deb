# Glossary

The terms boot2deb coins, and the standard terms it uses in a sense of its own. A term
that Debian's, the Linux kernel's, or U-Boot's documentation already defines is not
listed, so rootfs, initramfs, suite and pocket have no entry.

| Term | Definition | Source | Nearest standard term |
|---|---|---|---|
| axis | An independent dimension a build point is chosen along: the device, the kernel, the u-boot series, the suite, the layout, or the feature list. A recipe sets the axes it differs on, and the rest fall back to the device. | Coined | Build parameter |
| board profile | A `depthcharge-tools` codename for a firmware behavior set, such as the largest payload the firmware loads. It names the firmware a unit runs, not the board model. | depthcharge-tools | |
| boot method | The hardware layer that describes how a board's firmware loads the kernel (`boot-methods/<name>.toml`), such as `depthcharge`. Not U-Boot's bootmeth, which finds an operating system from inside U-Boot. | Coined | Boot flow |
| build point | One value on every axis, which a build turns into an image. A recipe names one, and a recipe plus a feature selection (`turing-rk1/forky+jellyfin`) names another without a new file. | Coined | Build configuration |
| capability | A free-form name one feature provides (`provides`) and another requires (`requires_capability`). A selection that requires a capability no selected feature provides is rejected at resolve. | Coined | Virtual package (Debian Policy) |
| device | The hardware layer for one board model (`devices/<board>.toml`). It names the board's SoC and boot method, and the kernels and suites the board supports. | Coined | Machine (Yocto) |
| feature | A named addition to the base image (`features/<name>.toml`): packages, files, and sometimes kernel configuration. A build selects any number of them. | Coined | Image feature (Yocto) |
| kmod | The config layer for one out-of-tree kernel module (`kmods/<name>.toml`), built as a `.deb` against the build's kernel. Not the `kmod` package of module tools. | Coined | Out-of-tree module (Linux kernel documentation) |
| layer | One of the config files a device resolves through, merged from arch to SoC to boot method to device. A later layer's value wins, so each states only what differs. | Coined | Yocto layer |
| lock | The file `update` writes beside a recipe (`<leaf>.lock`). It records the exact pin of every source the build depends on. | Coined | Cargo's `Cargo.lock` |
| overlay | A directory of config layers merged over the shipped tree (`--overlay <dir>`), where the later one wins. Not a layer's rootfs tree `overlay/`, which is laid into the image. | Coined | Buildroot's `BR2_EXTERNAL` tree |
| patch series | A named, ordered set of patches applied to a source tree before it compiles, defined by a manifest in the separate `patches` repository. A kernel definition names its series. | Linux kernel documentation | |
| pin | The exact revision of a source that a lock records: a commit for a git source, or a content hash for a vendored blob. Not apt pinning, which sets a package's priority. | Coined | Locked revision |
| press | The step that writes a built image to a file for flashing (`boot2deb press`), optionally personalized per unit. Pressing reuses the build's artifact rather than rebuilding it. | Coined | Image customization |
| recipe | A file that names one build point (`recipes/<device>/<leaf>.toml`). Any axis it leaves out is taken from the device, and its lock records the exact pins. | Coined | Yocto image recipe |
| seed | The small FAT partition every image has (`b2d-seed`), whose `seed.txt` first boot applies to the system. `press` writes it per unit. | Coined | cloud-init NoCloud seed |
| support claim | A recipe's `[support]` statement of how far its build point has been taken: `validated`, `expected` or `experimental`, with the date. It holds for the recipe's pins only. | Coined | Support tier |
