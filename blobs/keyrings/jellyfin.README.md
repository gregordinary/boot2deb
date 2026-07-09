# Jellyfin apt repository keyring

`jellyfin.gpg` verifies the `Release` signatures of the Jellyfin upstream apt
repository (`https://repo.jellyfin.org/debian`) that the `jellyfin` application
feature (`features/jellyfin.toml`) installs from. Vendored so the rootfs
bootstrap can verify the repo on any build host — the same
host-distro-independent convention as `debian-archive-keyring.gpg`.

- Provenance: fetched 2026-07-08 from
  `https://repo.jellyfin.org/jellyfin_team.gpg.key` (ASCII-armored, sha256
  `a0cde241ae297fa6f0265c0bf15ce9eb9ee97c008904a59ab367a67d59532839`) and
  dearmored with `gpg --dearmor`.
- Key: `Jellyfin Team <team@jellyfin.org>`, rsa3072,
  fingerprint `4918 AABC 486C A052 358D  778D 4902 3CD0 1DE2 1A7B`.
- sha256: `0bf79bc82f784381bdac9a3cf3731862db074967e77587a3bc0055576381c486`

Refreshing: when Jellyfin rotates its repository key, re-fetch the current key,
replace this file, and update this note — a deliberate re-validation event.
