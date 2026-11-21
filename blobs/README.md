# blobs

Trust anchors the build verifies against: the apt signing keyrings under
[`keyrings/`](keyrings/README.md), each with a fingerprint manifest that makes it
reviewable.

The Rockchip bootloader blobs (BL31 and DDR TPL) are not here. Each SoC layer names
its files and the repository they come from (`[rkbin]` and `[rkbin.upstream]`), the
recipe lock pins a commit of that repository and each file's sha256, and the u-boot
stage fetches exactly those files at that commit into `cache/rkbin/<commit>/`, refusing
any whose hash differs. `boot2deb build --blobs-dir <dir>` reads them from a local
directory instead, held to the same hashes, for an offline build. See
`THIRD-PARTY-NOTICES.md` at the repository root for their source and license.
