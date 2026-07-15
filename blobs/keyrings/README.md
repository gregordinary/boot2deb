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

## Fingerprint manifests

Each `<name>.gpg` ships a sibling **`<name>.fingerprints`** listing the primary
keys it is allowed to contain, one `<40-hex fingerprint>  <uid>` per line. The
build and `doctor` fail closed unless the keyring holds exactly that set.

A keyring is a trust anchor — it decides whose `Release` signatures the bootstrap
accepts — and as a binary blob it is the one vendored file a reviewer cannot read:
a diff shows `Bin 55918 -> 55934 bytes` and nothing about which keys changed. The
manifest moves the part that matters into reviewable text, so swapping a key is a
line in a pull request. It also makes the vendored copy *verifiable rather than
authoritative*: the fingerprints are published upstream, so anyone can check what
this repo trusts without trusting this repo.

Only primary keys are listed. A subkey cannot enter a certificate without a
binding signature from its primary key — which `gpgv` verifies during the
bootstrap, and which cannot be forged without the primary secret — so pinning the
primaries pins the whole keyring.

`boot2deb doctor <target>` prints every trust anchor a build uses and the vetted
keys in each. The check lives in `engine::keyring`; a shipped keyring that drifts
from its manifest fails `cargo test`.

## Adding one

1. Fetch the repository's signing key from its documented location (prefer an
   HTTPS origin the vendor controls).
2. Dearmor if ASCII-armored: `gpg --dearmor < key.asc > <name>.gpg`.
3. Record provenance beside it in `<name>.README.md` — the fetch URL, date, and
   the file's sha256; see the existing notes for the pattern.
4. Vet the keys against the vendor's published fingerprints, then write them to
   `<name>.fingerprints`:

   ```sh
   gpg --show-keys --with-colons --with-fingerprint <name>.gpg
   ```

Refreshing on a key rotation is a deliberate re-validation event: re-fetch the
key, re-vet the fingerprints, and update both the manifest and the provenance
note. Refreshing the keyring alone is a build failure, by design.
