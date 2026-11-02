# Vendored Ubuntu archive keyring

`ubuntu-archive-keyring.gpg` verifies the `Release`/`InRelease` signatures of the
Ubuntu suites the `ubuntu` distro layer serves: the build roots and the image rootfs
of an Ubuntu recipe both bootstrap against it. It is vendored for the reason the
Debian keyring is. A build host carries its own distribution's apt keys, so relying
on a host-installed keyring is not portable.

- Provenance: extracted from `ubuntu-keyring_2023.11.28.1build1_all.deb`, fetched on
  2026-11-02 from
  `http://archive.ubuntu.com/ubuntu/pool/main/u/ubuntu-keyring/`
  (`dpkg-deb -x`, path `usr/share/keyrings/ubuntu-archive-keyring.gpg`). The package's
  sha256 is `c377ccf26964f4c206c05c0bc7adb708e031beb919bb9a0ff63af983d064cd66`, as the
  signed `resolute` `Packages` index records it.
- sha256: `80a36b0a6de2f69f49d2df75ef473ccde121e9e190b9ea01d20a4f63778d5c31`
- Verifies the `resolute`, `resolute-updates`, `resolute-backports` and
  `resolute-security` `InRelease` files, each signed by the 2018 archive key
  `F6ECB3762474EDA9D21B7022871920D1991BC93C`. `resolute-security` verifies from both
  `security.ubuntu.com` and `snapshot.ubuntu.com`.

The keys it is allowed to contain are vetted in `ubuntu-archive-keyring.fingerprints`
and enforced on every build; see `README.md` in this directory.

Refreshing: when Ubuntu rotates archive keys, re-fetch the current `ubuntu-keyring`,
replace this file, and re-vet the new key's fingerprint into the manifest. That is a
deliberate re-validation event, like a kernel bump.
