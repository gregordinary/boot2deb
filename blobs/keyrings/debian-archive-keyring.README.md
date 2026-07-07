# Vendored Debian archive keyring

`debian-archive-keyring.gpg` verifies the `Release`/`InRelease` signatures of the
Debian suites the rootless build sandbox and (later) the image rootfs bootstrap
. It is vendored because the build host is frequently **not** Debian —
an Ubuntu, Pop!_OS, or Fedora host carries its own distro's apt keys, not Debian's,
so relying on a host-installed keyring is not portable. Vendoring keeps
the bootstrap reproducible, offline, and host-distro-independent, matching how the
rkbin blobs are vendored under `blobs/<soc>/`.

- Provenance: extracted from `debian-archive-keyring_2025.1_all.deb`, fetched from
  `http://deb.debian.org/debian/pool/main/d/debian-archive-keyring/`
  (`dpkg-deb -x`, path `usr/share/keyrings/debian-archive-keyring.gpg`).
- sha256: `506b815cbb32d9b6066b4a2aa524071e071761e7e7f68c3ac74f3061ba852017`
- Covers bullseye/bookworm/trixie primary keys; verifies the current `forky`
  `InRelease` (signed by the trixie/bookworm automatic signing subkeys).

Refreshing: when Debian rotates archive keys (a new release, rare), re-fetch the
current `debian-archive-keyring` and replace this file — a deliberate
re-validation event, like a kernel bump.
