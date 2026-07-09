# Vendored apt signing keyrings

Each `.gpg` file here is a binary (dearmored) OpenPGP keyring that verifies the
`Release`/`InRelease` signatures of one apt repository the build consumes:

- `debian-archive-keyring.gpg` — the Debian mirror (the rootless build sandbox
  and the image rootfs bootstrap).
- `jellyfin.gpg` — the Jellyfin upstream repository the `jellyfin` feature adds.

Keyrings are vendored because the build host is frequently not Debian — an
Ubuntu, Pop!_OS, or Fedora host carries its own distro's apt keys — and a
third-party repo's key is on no host at all. Vendoring keeps the bootstrap
reproducible, offline-friendly, and host-distro-independent, the same convention
as the rkbin blobs under `blobs/<soc>/`.

A feature that adds an `[[apt_sources]]` stanza names its keyring via
`signed_by = "<name>.gpg"`, which must exist here: `resolve`, `update`, and
`build` all preflight that existence, and the rootfs stage verifies the repo
against the key during the package solve (an unsigned source is never accepted).

Adding one:

1. Fetch the repository's signing key from its documented location (prefer an
   HTTPS origin the vendor controls).
2. Dearmor if ASCII-armored: `gpg --dearmor < key.asc > <name>.gpg`.
3. Record provenance beside it in `<name>.README.md` — the fetch URL, date, and
   the file's sha256; see the existing notes for the pattern.

Refreshing on a key rotation is a deliberate re-validation event: re-fetch the
key and update its provenance note.
