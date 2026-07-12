# Overlays

An **overlay** is an out-of-tree directory of config layers that boot2deb merges on top
of the shipped tree. It is how you keep *your* devices, recipes, and retunings in your
own repo — versioned, private, and never a fork of the vendored config. Pass one (or
several) with the global `--overlay <dir>` flag, on any command:

```sh
cargo run -p boot2deb-cli -- --overlay ~/my-boards build my-tablet-forky
```

Each `--overlay` must name an existing directory. An empty or mistyped path is a
resolve-time error rather than a silent no-op: an empty one would resolve every asset
against the current directory, and a typo would shadow nothing at all — either way the
build would quietly use a config tree you did not intend, which is exactly what an
overlay exists to make explicit.

An overlay has the **same directory layout** as the shipped root — any subset of
`devices/`, `socs/`, `arches/`, `boot-methods/`, `kernels/`, `features/`, `recipes/`,
plus `fragments/`, `blobs/`, and per-layer `overlay/` trees. You ship only the files you
add or change; everything else resolves from the shipped tree underneath. Because an
overlay is just a second config search root, everything the CLI does — `resolve`,
`doctor`, `verify-*`, `build`, and the `list-*` commands — sees the merged tree.

## What an overlay can do

- **Retune one value.** An overlay `devices/turing-rk1.toml` holding only
  `image_size = "8G"` changes that one field — every other key merges from the shipped
  file (see [merge semantics](#how-overlays-merge)).
- **Add to a list.** Add a `supported_kernel`, an extra rootfs package, another
  `[[apt_sources]]` — by restating the array with your addition (arrays are replaced
  wholesale, not concatenated).
- **Add a whole target.** Drop in a new `devices/my-tablet.toml`, `socs/…`, `kernels/…`,
  `features/…`, or `recipes/…`; it lists and builds alongside the shipped ones, since
  `list-devices`, `list-recipes`, and friends union the overlay's targets in.

## How overlays merge

The search path is the shipped root first, then each `--overlay` in the order given;
**later wins**, and any overlay wins over the shipped root. When the same layer file
(e.g. `devices/turing-rk1.toml`) exists in more than one root, the copies are
**deep-merged**:

- **Tables** merge key-by-key, recursing into nested tables — so setting one field
  leaves its siblings intact.
- **Scalars and arrays** are replaced **wholesale** — an overlay array *sets* the value,
  it does not append. To add one entry to a shipped list, restate the list with your
  entry included.

A layer file present only in an overlay simply adds a new target (nothing to merge).
Fragments, blobs, and per-feature/-layer `overlay/` trees resolve along the same path: a
same-named asset in an overlay shadows the shipped one, while feature/layer overlay trees
present in both roots stack (shipped first, overlay last).

## Locks land in the owning overlay

`update` writes a recipe's lock, and `build --save-manifest` writes its solved manifest,
into the **root that owns the recipe** — so an overlay recipe's lock and manifest land in
that overlay, beside the recipe, not in the shipped tree. An out-of-tree recipe stays
fully self-contained: recipe, lock, and manifest are all versioned together in your repo.

## The keyring is a fixed trust anchor

One asset an overlay may **not** silently replace: the Debian archive keyring
(`blobs/keyrings/debian-archive-keyring.gpg`). It is the trust root for the rootfs
bootstrap, so an overlay that ships its own copy is **refused** with a fail-closed error
rather than trusted — an overlay must not be able to swap the bootstrap's trust anchor.
If you genuinely intend to use the overlay's keyring, opt in explicitly with
`build --unsafe-overlay-keyring`. Every other asset follows the normal
highest-precedence-wins rule; only this trust anchor is pinned to the shipped root.

## Overlay or in-tree edit?

Two paths, chosen by intent:

- **Overlay** — you are bringing up your own board, or tuning a build for yourself. Keep
  it out-of-tree with `--overlay`; there is nothing to upstream and nothing to fork.
- **In-tree edit** — you are contributing a board back to boot2deb. Edit the vendored
  tree directly and open a pull request.

[Adding a board](../contributing/adding-a-board.md) walks through the layers to write and
applies to both paths — the only difference is whether the files land in your overlay or
in the vendored tree.
