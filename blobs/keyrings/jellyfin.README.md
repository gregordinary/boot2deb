# Jellyfin apt repository keyring (not vendored yet)

The `jellyfin` application feature (`features/jellyfin.toml`) installs Jellyfin
from its signed upstream apt repository (`https://repo.jellyfin.org/debian`). That
repository's signing key must be vendored here as `jellyfin.gpg` so the rootfs
bootstrap can verify its `Release` signatures — the same host-distro-independent
convention as `debian-archive-keyring.gpg`.

**Status: not yet vendored.** The `jellyfin` feature and the
`turing-rk1-jellyfin` recipe resolve and validate today (`boot2deb resolve`), but
the engine-side activation of third-party apt sources inside the mmdebstrap
bootstrap — and this key — land with the physical boot gate (see
`features/jellyfin/README.md`). Vendoring, when it lands:

- Fetch the current key from `https://repo.jellyfin.org/jellyfin_team.gpg.key`
  (ASCII-armored) and dearmor it: `gpg --dearmor < jellyfin_team.gpg.key >
  jellyfin.gpg`.
- Record its sha256 here and pin it, matching the Debian keyring's provenance
  note. Refreshing on a key rotation is a deliberate re-validation event.
