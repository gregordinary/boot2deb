# Vendored apt signing keyrings

Each `.gpg` file here is a binary (dearmored) OpenPGP keyring that verifies the
`Release`/`InRelease` signatures of one apt repository the build consumes:

- `debian-archive-keyring.gpg` — the Debian archive, for every recipe whose suite
  the `debian` distro layer serves (the build roots and the image rootfs bootstrap).
- `ubuntu-archive-keyring.gpg` — the Ubuntu archive, for every recipe whose suite the
  `ubuntu` distro layer serves.
- `jellyfin.gpg` — the Jellyfin upstream repository the `jellyfin` feature adds.

Keyrings are vendored because the build host is frequently not Debian — an
Ubuntu, Pop!_OS, or Fedora host has its own distro's apt keys — and a
third-party repo's key is on no host at all. Vendoring keeps the bootstrap
reproducible, offline-friendly, and host-distro-independent, the same convention
as the rkbin blobs under `blobs/<soc>/`.

A feature that adds an `[[apt_sources]]` stanza names its keyring via
`signed_by = "<name>.gpg"`, which must exist here: `resolve`, `update`, and
`build` all preflight that existence, and the rootfs stage verifies the repo
against the key during the package solve (an unsigned source is never accepted).

**This directory is the whole of the search.** `signed_by` is a bare file name —
`[A-Za-z0-9._-]`, no separators and no `.`/`..` — so a stanza cannot name a key
anywhere else, and the resolved file is checked to still be *inside* a keyring
directory after symlinks are followed, so a link planted here cannot make an
arbitrary host file the anchor a repository is verified against. An overlay may
vendor its own keyring in its own `blobs/keyrings/` for a repository it adds. A
distro layer's archive keyring is the exception, resolved from the shipped tree
only, and an overlay shipping a copy of one is refused as a swap rather than
preferred (`--unsafe-overlay-keyring` is the explicit opt-in).

## Fingerprint manifests

Each `<name>.gpg` ships a sibling **`<name>.fingerprints`** listing the primary
keys it is allowed to contain, one `<fingerprint>  <uid>` per line, the fingerprint
40 hex digits for a version 4 key and 64 for a version 6 one. The
build and `doctor` fail closed unless the keyring holds exactly that set.

A keyring is a trust anchor — it decides whose `Release` signatures the bootstrap
accepts — and as a binary blob it is the one vendored file a reviewer cannot read:
a diff shows `Bin 55918 -> 55934 bytes` and nothing about which keys changed. The
manifest moves the part that matters into reviewable text, so swapping a key is a
line in a pull request. It also makes the vendored copy *verifiable rather than
authoritative*: the fingerprints are published upstream, so anyone can check what
this repo trusts without trusting this repo.

Only primary keys are listed. A subkey cannot enter a certificate without a
binding signature from its primary key — which the bootstrap's signature verifier
checks, and which cannot be forged without the primary secret — so pinning the
primaries pins the whole keyring.

Each vetted certificate must also hold a key that verifier can use: an RSA key of
2048 bits or more, or an Ed25519 key. A certificate holding only DSA, ECDSA or Ed448
keys, or smaller RSA ones, can never vouch for a release, and the check refuses it.

`boot2deb doctor <target>` prints every trust anchor a build uses and the vetted
keys in each. The check lives in `engine::keyring`. A shipped keyring that drifts
from its manifest, or pins a certificate the verifier cannot use, fails `cargo test`.

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
