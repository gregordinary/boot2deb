//! Reads typed config layers from a boot2deb config root (the repo directory
//! holding `devices/`, `socs/`, `arches/`, `boot-methods/`, `kernels/`, `kmods/`,
//! `features/`, `recipes/`).
//!
//! **Layer search path & overlays.** A [`ConfigRoot`] holds an *ordered*
//! search path: the shipped root first, then zero or more out-of-tree overlay
//! directories (`--overlay <dir>`), later ones winning. A layer file present only
//! in an overlay adds a new target; a file present under a shipped name is
//! **deep-merged last-wins** over the shipped one — tables merge key-by-key with
//! the overlay winning, while a scalar or array key is replaced wholesale (the
//! simplest predictable last-wins). Each layer file is parsed to a
//! [`toml::Value`], the values are merged across the path, and the merged value is
//! deserialized into the strict `deny_unknown_fields` struct, so validation is
//! unchanged and the authored structs stay untouched. This lets a user retune one
//! device's `image_size` or add a `supported_kernel` — or drop in a whole new
//! device/soc/kernel — without forking the vendored config.
//!
//! **Device variants.** A device may also name another device as its parent
//! ([`extends`](DeviceLayer::extends)), which is merged by the same rules along a
//! second axis: the `extends` chain is flattened base-most-first, then the search
//! path merges over the result. So a variant board states only its deltas, and an
//! overlay can still retune either the variant or what it extends. See
//! [`device_with_lineage`](ConfigRoot::device_with_lineage) for the asset ordering
//! that falls out of it.

use crate::error::ConfigError;
use crate::model::*;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// A boot2deb config root — an ordered search path of directories, each holding
/// the config-layer subtrees. Lookups walk the path; overlays (later entries) win
/// over the shipped root (first entry). Tests and alternate checkouts just point
/// at a different path.
pub struct ConfigRoot {
    /// Low→high precedence: `roots[0]` is the shipped/primary root; later entries
    /// are overlays that win on merge and on single-file lookup.
    roots: Vec<PathBuf>,
}

impl ConfigRoot {
    /// Wrap a single directory as a config root (no overlays). Does not touch the
    /// filesystem; missing files surface as [`ConfigError::NotFound`] on lookup.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            roots: vec![root.into()],
        }
    }

    /// A primary (shipped) root plus ordered overlay directories. Overlays
    /// are listed low→high: a later overlay wins over an earlier one, and any
    /// overlay wins over the primary root.
    ///
    /// Each overlay must be an existing directory. An empty path would silently
    /// resolve every asset against the process's current directory, and a mistyped
    /// one would shadow nothing at all — in both cases the build proceeds against a
    /// config tree the operator did not intend, which is precisely the failure an
    /// overlay exists to make explicit. Both are [`ConfigError::InvalidOverlay`].
    pub fn with_overlays(
        root: impl Into<PathBuf>,
        overlays: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, ConfigError> {
        let mut roots = vec![root.into()];
        for overlay in overlays {
            let why = if overlay.as_os_str().is_empty() {
                Some("the path is empty")
            } else if !overlay.exists() {
                Some("no such directory")
            } else if !overlay.is_dir() {
                Some("not a directory")
            } else {
                None
            };
            if let Some(why) = why {
                return Err(ConfigError::InvalidOverlay {
                    path: overlay.display().to_string(),
                    why,
                });
            }
            roots.push(overlay);
        }
        Ok(Self { roots })
    }

    /// The primary (shipped) root — the base of the search path. Non-config assets
    /// resolved by direct join (blobs, fragments, overlay trees) start here; use
    /// [`find_asset`](Self::find_asset) to make those overlay-aware.
    pub fn path(&self) -> &Path {
        &self.roots[0]
    }

    /// The full search path — the primary root followed by every overlay, in
    /// low→high precedence order. For a consumer that must confirm an asset it
    /// resolved stays *within* the config tree (containment), not just find it.
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The highest-precedence existing path for a repo-relative asset (a fragment,
    /// blob, or overlay tree that is *not* a merged config layer), or `None` if no
    /// root has it. Searched high→low so an overlay's copy shadows the shipped one.
    pub fn find_asset(&self, rel: impl AsRef<Path>) -> Option<PathBuf> {
        let rel = rel.as_ref();
        self.roots
            .iter()
            .rev()
            .map(|r| r.join(rel))
            .find(|p| p.exists())
    }

    /// Resolve a *trust anchor* asset (the Debian archive keyring) that overlays
    /// must not be able to swap. Unlike [`find_asset`](Self::find_asset),
    /// which lets the highest-precedence overlay win, this resolves from the primary
    /// (shipped) root only and treats an overlay copy as a fail-closed error:
    ///  - `Ok(Some(path))` — the shipped root's copy (no overlay ships one, or
    ///    `allow_overlay` is set — the explicit `--unsafe-overlay-keyring` opt-in,
    ///    in which case the highest-precedence copy wins like `find_asset`).
    ///  - `Ok(None)` — no root has it (the caller falls back to the host trust store).
    ///  - `Err(OverlayTrustAnchor)` — an overlay ships the asset and `allow_overlay`
    ///    is false: a swap attempt, refused rather than silently trusted.
    pub fn find_trust_anchor(
        &self,
        rel: impl AsRef<Path>,
        allow_overlay: bool,
    ) -> Result<Option<PathBuf>, ConfigError> {
        let rel = rel.as_ref();
        if allow_overlay {
            // Opted into the overlay explicitly: highest-precedence copy wins.
            return Ok(self.find_asset(rel));
        }
        // An overlay (any non-primary root) shipping the anchor is a swap attempt.
        if self.roots[1..].iter().any(|r| r.join(rel).exists()) {
            return Err(ConfigError::OverlayTrustAnchor {
                asset: rel.display().to_string(),
            });
        }
        let shipped = self.roots[0].join(rel);
        Ok(shipped.exists().then_some(shipped))
    }

    /// Every existing path for a repo-relative asset across the search path, in
    /// low→high precedence order — for assets that *stack* rather than shadow (a
    /// feature/layer overlay tree present in both the shipped root and an overlay,
    /// merged shipped-first so the overlay wins the last-writer semantics).
    pub fn find_asset_all(&self, rel: impl AsRef<Path>) -> Vec<PathBuf> {
        let rel = rel.as_ref();
        self.roots
            .iter()
            .map(|r| r.join(rel))
            .filter(|p| p.exists())
            .collect()
    }

    /// The root that *owns* a layer file — the highest-precedence root containing
    /// `<subdir>/<name>.toml`, or the primary root if none does. A write target
    /// derived from a layer (a recipe's lock) lands beside the file it belongs to,
    /// so `update` on an overlay recipe writes into that overlay, not the shipped
    /// tree.
    fn owning_root(&self, subdir: &str, name: &str) -> &Path {
        let rel = format!("{subdir}/{name}.toml");
        self.roots
            .iter()
            .rev()
            .find(|r| r.join(&rel).exists())
            .unwrap_or(&self.roots[0])
    }

    /// Read a config file to a string. A missing file is `Ok(None)` (the caller
    /// walks the rest of the search path and decides if it is a real
    /// [`ConfigError::NotFound`]); any other read failure is [`ConfigError::Io`].
    fn read_file(path: &Path) -> Result<Option<String>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Load a config layer, deep-merging every copy of `<subdir>/<name>.toml`
    /// found along the search path (shipped → overlays, overlay wins), then
    /// deserializing the merged value into `T`. Missing in *every* root is
    /// [`ConfigError::NotFound`].
    fn load<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        subdir: &str,
        name: &str,
    ) -> Result<T, ConfigError> {
        validate_name(kind, name)?;
        let rel = format!("{subdir}/{name}.toml");
        self.load_merged(kind, name, &rel)
    }

    /// Shared merge-and-deserialize over a repo-relative path, used by both the
    /// subdir layers ([`load`](Self::load)) and root-level `base.toml`.
    fn load_merged<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        name: &str,
        rel: &str,
    ) -> Result<T, ConfigError> {
        let (value, path) = self.merge_value(kind, name, rel)?;
        deserialize_at(value, &path)
    }

    /// Deep-merge every copy of `rel` along the search path into one
    /// [`toml::Value`], returning it with the highest-precedence path that
    /// contributed (so a later parse error is attributed to a real file).
    ///
    /// Kept separate from [`load_merged`](Self::load_merged) for the layers whose
    /// Rust *type* is chosen by a value inside the merged config — a boot method by
    /// its filename, a kernel by its `flavor` — which must inspect the value before
    /// deserializing it into the right variant.
    fn merge_value(
        &self,
        kind: &'static str,
        name: &str,
        rel: &str,
    ) -> Result<(toml::Value, PathBuf), ConfigError> {
        let mut merged: Option<toml::Value> = None;
        let mut top_path: Option<PathBuf> = None;
        let mut last_path = PathBuf::new();
        for root in &self.roots {
            let path = root.join(rel);
            last_path = path.clone();
            let Some(text) = Self::read_file(&path)? else {
                continue;
            };
            let value: toml::Value =
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
            top_path = Some(path);
            merged = Some(match merged {
                Some(mut base) => {
                    merge_toml(&mut base, value);
                    base
                }
                None => value,
            });
        }
        let Some(value) = merged else {
            return Err(ConfigError::NotFound {
                kind,
                name: name.to_string(),
                path: last_path.display().to_string(),
                // The subdir `rel` lives under is the inventory this name should have
                // come from. `base.toml` has none, and then there is nothing to
                // suggest — which is right: it is not a name a user typed.
                similar: match rel.split_once('/') {
                    Some((subdir, _)) => self.near_names(subdir, name),
                    None => Vec::new(),
                },
            });
        };
        Ok((value, top_path.unwrap_or(last_path)))
    }

    /// Load `devices/<name>.toml`, resolving its [`extends`](DeviceLayer::extends)
    /// chain.
    pub fn device(&self, name: &str) -> Result<DeviceLayer, ConfigError> {
        Ok(self.device_with_lineage(name)?.0)
    }

    /// Load a device together with its lineage — the device and its
    /// [`extends`](DeviceLayer::extends) ancestors, **base-most first**, ending with
    /// `name` itself. A device that extends nothing has a one-entry lineage.
    ///
    /// The lineage is the order the devices' assets stack in, which is why it is
    /// returned rather than recomputed: the merged [`DeviceLayer`] cannot express it
    /// (a merge collapses the chain to one value), and the overlay trees of every
    /// device in it are laid into the rootfs in this order, so a variant's files win
    /// over the parent's.
    pub fn device_with_lineage(
        &self,
        name: &str,
    ) -> Result<(DeviceLayer, Vec<String>), ConfigError> {
        // Walk child -> base-most, collecting each device's own merged value. The
        // chain is bounded by the cycle check: every step either finds a device not
        // yet seen or fails.
        let mut lineage = Vec::new();
        let mut chain: Vec<(toml::Value, PathBuf)> = Vec::new();
        let mut current = name.to_string();
        loop {
            validate_name("device", &current)?;
            if lineage.contains(&current) {
                lineage.push(current);
                return Err(ConfigError::DeviceExtendsCycle {
                    device: name.to_string(),
                    chain: lineage.join(" -> "),
                });
            }
            let rel = format!("devices/{current}.toml");
            let (mut value, path) = self.merge_value("device", &current, &rel)?;
            // `extends` is read here, before deserialization, so the chain can be
            // walked at all; a non-string is named against the file that holds it
            // rather than surfacing as a type error on a merged value.
            let parent = match value.get("extends") {
                None => None,
                Some(toml::Value::String(p)) => Some(p.clone()),
                Some(other) => {
                    return Err(ConfigError::InvalidDeviceExtends {
                        device: current,
                        found: other.type_str(),
                    })
                }
            };
            // Only the named device's own `extends` should survive the merge, so an
            // ancestor's does not read as this device's parent.
            if !chain.is_empty() {
                if let toml::Value::Table(t) = &mut value {
                    t.remove("extends");
                }
            }
            lineage.push(current);
            chain.push((value, path));
            match parent {
                Some(p) => current = p,
                None => break,
            }
        }

        // Merge base-most -> child, so a variant wins over what it extends. Errors are
        // attributed to the named device's file: it is the one the operator authored,
        // and after the merge a bad value cannot be traced to an ancestor anyway.
        let (mut merged, mut top_path) = chain.pop().expect("the walk pushes at least once");
        while let Some((value, path)) = chain.pop() {
            merge_toml(&mut merged, value);
            top_path = path;
        }
        lineage.reverse();
        Ok((deserialize_at(merged, &top_path)?, lineage))
    }
    /// Load the SoC layer for `soc` (`socs/<soc>.toml`).
    pub fn soc(&self, soc: Soc) -> Result<SocLayer, ConfigError> {
        self.load("soc", "socs", soc.as_str())
    }
    /// Load the arch layer for `arch` (`arches/<arch>.toml`).
    pub fn arch(&self, arch: Arch) -> Result<ArchLayer, ConfigError> {
        self.load("arch", "arches", arch.as_str())
    }
    /// Load the boot-method layer for `bm` (`boot-methods/<bm>.toml`).
    ///
    /// The [`BootMethod`] *is* the variant selector, so the file is deserialized
    /// straight into that method's struct. Fields belonging to another method are
    /// unknown fields here and are rejected — an `idbloader_offset` in
    /// `boot-methods/depthcharge.toml` is a parse error naming the file, not a value
    /// silently carried into a build that has no raw gap to write it to.
    pub fn boot_method(&self, bm: BootMethod) -> Result<BootMethodLayer, ConfigError> {
        let rel = format!("boot-methods/{}.toml", bm.as_str());
        let (value, path) = self.merge_value("boot-method", bm.as_str(), &rel)?;
        Ok(match bm {
            BootMethod::RockchipRkbin => {
                BootMethodLayer::RockchipRkbin(deserialize_at(value, &path)?)
            }
            BootMethod::Depthcharge => BootMethodLayer::Depthcharge(deserialize_at(value, &path)?),
        })
    }

    /// Load `kernels/<id>.toml`, dispatching on its `flavor` to the variant that
    /// flavor's fields belong to: a compiled kernel carries a source ref, defconfig,
    /// fragments, and patch series; a `distro-package` kernel carries only a package
    /// name. Each variant is strict, so a fragment list on a distro kernel — which
    /// nothing would ever read — fails at load rather than being ignored.
    pub fn kernel(&self, id: &str) -> Result<KernelDef, ConfigError> {
        validate_name("kernel", id)?;
        let rel = format!("kernels/{id}.toml");
        let (value, path) = self.merge_value("kernel", id, &rel)?;
        let flavor: KernelFlavor = value
            .get("flavor")
            .cloned()
            .ok_or_else(|| ConfigError::MissingKernelFlavor {
                kernel: id.to_string(),
                path: path.display().to_string(),
            })?
            .try_into()
            .map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        Ok(match flavor {
            KernelFlavor::Mainline | KernelFlavor::Vendor => {
                KernelDef::Compiled(deserialize_at(value, &path)?)
            }
            KernelFlavor::DistroPackage => KernelDef::Distro(deserialize_at(value, &path)?),
        })
    }
    /// Load a recipe by its `<device>/<leaf>` reference
    /// (`recipes/<device>/<leaf>.toml`). The one config key that carries a directory
    /// separator: a recipe lives under its device's folder, and the reference is the
    /// path to it (minus the extension), so the leaf drops the redundant device
    /// prefix — `turing-rk1/media-accel-forky`, not `turing-rk1-media-accel-forky`.
    /// The reference admits at most one interior `/`, both halves bare identifiers
    /// with no `.`/`..`, absolute, or repeated separator — so it cannot traverse out
    /// of `recipes/`.
    pub fn recipe(&self, name: &str) -> Result<Recipe, ConfigError> {
        validate_recipe_ref(name)?;
        let rel = format!("recipes/{name}.toml");
        self.load_merged("recipe", name, &rel)
    }

    /// Load `features/<name>.toml` — a composable rootfs feature.
    pub fn feature(&self, name: &str) -> Result<crate::feature::Feature, ConfigError> {
        self.load("feature", "features", name)
    }

    /// Load `kmods/<name>.toml` — one out-of-tree kernel-module set a device selects by
    /// name. The stem *is* the kmod's identity (the layer carries no `name` field), so
    /// the caller pairs the two into a [`ResolvedKmod`].
    pub fn kmod(&self, name: &str) -> Result<KmodLayer, ConfigError> {
        self.load("kmod", "kmods", name)
    }

    /// Load `base.toml` — the distro-generic rootfs substrate. Unlike the
    /// other layers it is a single file at each root, not a named file in a subdir;
    /// it deep-merges across the search path like the rest.
    pub fn base(&self) -> Result<BaseLayer, ConfigError> {
        self.load_merged("base", "base", "base.toml")
    }

    /// Load `recipes/<name>.lock` — the resolved exact pins for a recipe.
    /// `boot2deb build` reads only this; `boot2deb update` writes it. A lock is an
    /// *atomic* artifact (exact pins), not a mergeable layer, and it is read from
    /// the root that **owns the recipe** — the same root
    /// [`lock_path`](Self::lock_path) writes to — so `update`'s write target and
    /// `build`'s read source can never address two different locks for one
    /// recipe. An overlay that wants different pins overlays the recipe
    /// and its lock as a unit; an overlay retuning a shipped recipe owns both
    /// automatically.
    pub fn lock(&self, name: &str) -> Result<crate::lock::Lock, ConfigError> {
        validate_build_ref(name)?;
        let path = self
            .owning_root("recipes", recipe_half(name))
            .join("recipes")
            .join(format!("{name}.lock"));
        match Self::read_file(&path)? {
            Some(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            }),
            None => Err(ConfigError::NotFound {
                kind: "lock",
                name: name.to_string(),
                path: path.display().to_string(),
                // Recipes, not locks: a lock is derived from a recipe, so a name that
                // has no lock is either a typo for a recipe or a recipe awaiting
                // `update`, and both are answered by the recipe inventory.
                similar: crate::error::similar_names(
                    name,
                    &self.list_recipes().unwrap_or_default(),
                ),
            }),
        }
    }

    /// Filesystem path of `recipes/<name>.lock`, whether or not it exists — the
    /// target `boot2deb update` writes to. The lock lands in the root that *owns*
    /// the recipe (an overlay recipe's lock goes into that overlay), or the primary
    /// root if the recipe is not on the path. The name is validated first, since
    /// this is a *write* target: an unchecked `../` or absolute name would let
    /// `update` clobber a file outside `recipes/`.
    pub fn lock_path(&self, name: &str) -> Result<PathBuf, ConfigError> {
        validate_build_ref(name)?;
        Ok(self
            .owning_root("recipes", recipe_half(name))
            .join("recipes")
            .join(format!("{name}.lock")))
    }

    /// Filesystem path of a file that lives beside `recipe` in the recipe's own
    /// directory (`recipes/<device>/<filename>`) — e.g. that recipe's committed solved
    /// package manifest, next to its `.toml` and `.lock`. Anchored to the root that
    /// *owns* `recipe`, the same way [`lock_path`](Self::lock_path) is, so an overlay
    /// recipe's manifest lands in that overlay beside its lock rather than diverging
    /// into the primary root. `recipe` is validated as a recipe reference (a single
    /// `<device>/<leaf>` separator, no traversal) and `filename` as a bare name (no
    /// separator at all), since this is a *write* target: an unchecked `../` or
    /// absolute component would let `build --save-manifest` write outside `recipes/`.
    pub fn recipe_sibling(&self, recipe: &str, filename: &str) -> Result<PathBuf, ConfigError> {
        validate_build_ref(recipe)?;
        validate_name("manifest", filename)?;
        // The recipe's own directory: the parent of `recipes/<device>/<leaf>.toml`,
        // i.e. `recipes/<device>` (or `recipes/` for a bare, un-nested reference).
        let owning = self.owning_root("recipes", recipe_half(recipe));
        let recipe_rel = format!("recipes/{recipe}.toml");
        let dir = Path::new(&recipe_rel)
            .parent()
            .expect("recipes/<ref>.toml always has a parent");
        Ok(owning.join(dir).join(filename))
    }

    /// Names under `subdir` close enough to `name` to be the one that was meant, for a
    /// [`ConfigError::NotFound`]'s "did you mean" hint.
    ///
    /// Best-effort: an unreadable directory yields no suggestions rather than an
    /// error, because this decorates a failure that has already happened and must not
    /// replace it with a different one.
    fn near_names(&self, subdir: &str, name: &str) -> Vec<String> {
        // Recipes nest one level under their device folder, so they have their own
        // lister; every other layer is a flat directory of `<name>.toml`.
        let candidates = if subdir == "recipes" {
            self.list_recipes()
        } else {
            self.list(subdir)
        };
        crate::error::similar_names(name, &candidates.unwrap_or_default())
    }

    /// Stems of every `*.toml` in `subdir`, unioned across the search path, sorted
    /// and de-duplicated — so an overlay's targets list alongside the shipped ones,
    /// and a target present in both (an overlay retuning a shipped device) appears
    /// once. An absent directory in a root contributes nothing; any *other*
    /// `read_dir` failure (a wrong/unreadable root, a permission error) is surfaced
    /// as [`ConfigError::Io`] rather than silently yielding a success exit.
    pub fn list(&self, subdir: &str) -> Result<Vec<String>, ConfigError> {
        let mut names = std::collections::BTreeSet::new();
        for root in &self.roots {
            let dir = root.join(subdir);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ConfigError::Io {
                        path: dir.display().to_string(),
                        source,
                    })
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Recipe references across the search path, as `<device>/<leaf>` strings, sorted
    /// and de-duplicated. Unlike [`list`](Self::list) — which is a flat single-level
    /// scan for the strictly-flat layers (devices, socs, kernels, features) — recipes
    /// nest one level under their device's folder (`recipes/<device>/<leaf>.toml`), so
    /// this descends exactly one level: each `recipes/<device>/` directory contributes
    /// its `*.toml` stems as `<device>/<stem>`. Non-`.toml` siblings (`.lock`,
    /// `.pkgs.lock`) and any deeper sidecar subdirectory are ignored; a stray
    /// top-level `recipes/*.toml` is listed by its bare stem for robustness, though the
    /// shipped layout nests every recipe. An overlay's recipes union with the shipped
    /// ones, a reference present in both appears once, and an absent `recipes/`
    /// contributes nothing while any other read failure is [`ConfigError::Io`].
    pub fn list_recipes(&self) -> Result<Vec<String>, ConfigError> {
        let mut names = std::collections::BTreeSet::new();
        for root in &self.roots {
            let base = root.join("recipes");
            let entries = match std::fs::read_dir(&base) {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ConfigError::Io {
                        path: base.display().to_string(),
                        source,
                    })
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // A device directory: emit `<device>/<leaf>` for each recipe toml.
                    let Some(device) = path.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    let leaves = match std::fs::read_dir(&path) {
                        Ok(leaves) => leaves,
                        Err(source) => {
                            return Err(ConfigError::Io {
                                path: path.display().to_string(),
                                source,
                            })
                        }
                    };
                    for leaf in leaves.flatten() {
                        let lp = leaf.path();
                        if lp.extension().and_then(|e| e.to_str()) == Some("toml") {
                            if let Some(stem) = lp.file_stem().and_then(|s| s.to_str()) {
                                names.insert(format!("{device}/{stem}"));
                            }
                        }
                    }
                } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
        Ok(names.into_iter().collect())
    }
}

/// Deserialize a merged config value into its strict struct, attributing any
/// failure to the file it came from. Shared by the plain loaders and the
/// variant-dispatching ones, so an unknown field is reported identically either way.
fn deserialize_at<T: DeserializeOwned>(value: toml::Value, path: &Path) -> Result<T, ConfigError> {
    value.try_into().map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Deep-merge `overlay` into `base` with the overlay winning. Two tables
/// merge key-by-key (recursing into nested tables); anything else — a scalar, an
/// array, or a type mismatch between the two sides — replaces `base` wholesale.
/// This is the simplest predictable last-wins: a table grows/overrides field by
/// field, while an array or scalar key is set, not concatenated.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match overlay {
        toml::Value::Table(over) => {
            if let toml::Value::Table(under) = base {
                for (key, over_val) in over {
                    match under.get_mut(&key) {
                        Some(under_val) => merge_toml(under_val, over_val),
                        None => {
                            under.insert(key, over_val);
                        }
                    }
                }
            } else {
                // `base` is a scalar/array but the overlay is a table → replace.
                *base = toml::Value::Table(over);
            }
        }
        other => *base = other,
    }
}

/// Whether `s` is a bare identifier safe to join into a filesystem path: non-empty,
/// no leading dot (excludes hidden files, `.`, and `..`), and drawn only from
/// `[A-Za-z0-9._-]` — which excludes every path separator. This is the atom both
/// [`validate_name`] (one such atom) and [`validate_recipe_ref`] (two, joined by a
/// single `/`) are built from.
fn is_bare_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Reject any name that is not a single bare identifier before it joins into a
/// filesystem path — so a config cross-reference or CLI argument can never traverse
/// out of the config root. Used for the strictly-flat layers (devices, socs, arches,
/// boot-methods, kernels, features) and for a manifest *filename*; recipe references
/// go through [`validate_recipe_ref`] instead.
fn validate_name(kind: &'static str, name: &str) -> Result<(), ConfigError> {
    if is_bare_name(name) {
        Ok(())
    } else {
        Err(ConfigError::InvalidName {
            kind,
            name: name.to_string(),
        })
    }
}

/// Reject any recipe reference that is not `<device>/<leaf>` (or a bare `<leaf>`)
/// before it joins into a filesystem path. Recipes are the one config layer that
/// nests one level under a device folder, so a reference may carry a *single*
/// interior `/` separating two [bare identifiers](is_bare_name); each half is held to
/// the same rule [`validate_name`] enforces. Because each segment must be bare and
/// non-empty, this rejects a leading/trailing/absolute/doubled slash, more than one
/// slash, and any `.` or `..` segment — so the reference can never traverse out of
/// `recipes/` when joined into `recipes/<ref>.toml`, its `.lock`, or its manifest.
fn validate_recipe_ref(name: &str) -> Result<(), ConfigError> {
    let mut segments = name.split('/');
    let ok = match (segments.next(), segments.next(), segments.next()) {
        (Some(leaf), None, _) => is_bare_name(leaf),
        (Some(device), Some(leaf), None) => is_bare_name(device) && is_bare_name(leaf),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ConfigError::InvalidRecipeRef {
            name: name.to_string(),
        })
    }
}

/// [`validate_recipe_ref`] as a crate-visible check, for
/// [`BuildPoint`](crate::buildpoint::BuildPoint) to hold its recipe half to exactly
/// the rule the loader enforces rather than restating it.
pub(crate) fn check_recipe_ref(name: &str) -> Result<(), ConfigError> {
    validate_recipe_ref(name)
}

/// [`validate_name`] for a feature, for
/// [`BuildPoint`](crate::buildpoint::BuildPoint) to hold each feature in a reference
/// to the same bare-identifier rule — which is what keeps a reference's `+` suffix
/// from opening a path the recipe half could not.
pub(crate) fn check_feature_name(name: &str) -> Result<(), ConfigError> {
    validate_name("feature", name)
}

/// Validate a *build reference* — a recipe reference optionally carrying a
/// `+`-separated feature suffix, the form
/// [`BuildPoint::reference`](crate::buildpoint::BuildPoint::reference) produces.
///
/// Used by the write targets that a variant build derives ([`ConfigRoot::lock_path`],
/// [`ConfigRoot::lock`], [`ConfigRoot::recipe_sibling`]), because those name the
/// build point rather than the authored recipe. [`ConfigRoot::recipe`] keeps the
/// stricter rule: a `.toml` exists for a recipe, never for a variant.
fn validate_build_ref(name: &str) -> Result<(), ConfigError> {
    let mut parts = name.split(crate::buildpoint::FEATURE_SEP);
    validate_recipe_ref(parts.next().unwrap_or_default())?;
    for feature in parts {
        validate_name("feature", feature)?;
    }
    Ok(())
}

/// The recipe half of a build reference — everything before the first
/// [`FEATURE_SEP`](crate::buildpoint::FEATURE_SEP).
///
/// A variant's lock and manifest live beside the recipe they derive from, so the
/// root that *owns* them is the root owning that recipe, not one owning a `.toml`
/// that does not exist.
fn recipe_half(name: &str) -> &str {
    name.split(crate::buildpoint::FEATURE_SEP)
        .next()
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_pass() {
        for n in [
            "turing-rk1",
            "turing-rk1-forky",
            "rk3588-mainline-7.1",
            "a_b.c",
        ] {
            assert!(validate_name("device", n).is_ok(), "{n} should be valid");
        }
    }

    #[test]
    fn traversal_and_absolute_names_are_rejected() {
        for n in [
            "",            // empty
            "..",          // parent
            ".hidden",     // leading dot
            "a/b",         // separator
            "../etc/x",    // traversal
            "/etc/passwd", // absolute
            "a\\b",        // backslash
            "a b",         // space
            "a:b",         // colon
        ] {
            assert!(
                matches!(
                    validate_name("device", n),
                    Err(ConfigError::InvalidName { .. })
                ),
                "{n:?} should be rejected"
            );
        }
    }

    #[test]
    fn recipe_ref_allows_one_nested_segment_and_rejects_traversal() {
        // A bare leaf or a single `<device>/<leaf>` boundary is accepted.
        for ok in [
            "forky",
            "turing-rk1/forky",
            "turing-rk1/media-accel-forky",
            "h96-max-m9/console-forky",
        ] {
            assert!(
                validate_recipe_ref(ok).is_ok(),
                "{ok:?} should be a valid recipe ref"
            );
        }
        // Anything that could traverse out of `recipes/` is rejected: empty, a second
        // separator, a leading/trailing/absolute/doubled slash, and dot segments.
        for bad in [
            "",                    // empty
            "/forky",              // absolute / leading slash
            "turing-rk1/",         // trailing slash
            "a//b",                // doubled slash (empty middle segment)
            "a/b/c",               // more than one separator
            "turing-rk1/../etc",   // dot-dot segment
            "turing-rk1/.hidden",  // leading-dot leaf
            "../turing-rk1/forky", // traversal
            "a\\b",                // backslash
            "a b/forky",           // space
        ] {
            assert!(
                matches!(
                    validate_recipe_ref(bad),
                    Err(ConfigError::InvalidRecipeRef { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn lock_path_rejects_traversal() {
        let root = ConfigRoot::new("/cfg");
        assert!(root.lock_path("turing-rk1/forky").is_ok());
        assert!(matches!(
            root.lock_path("../../etc/cron.d/x"),
            Err(ConfigError::InvalidRecipeRef { .. })
        ));
    }

    // ---- TOML deep-merge algebra --------------------------------------

    fn val(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn merge_deep_merges_tables_key_by_key() {
        let mut base = val("a = 1\n[t]\nx = 1\ny = 1\n");
        merge_toml(&mut base, val("b = 2\n[t]\ny = 9\nz = 3\n"));
        // Top-level: base `a` kept, overlay `b` added.
        assert_eq!(base["a"].as_integer(), Some(1));
        assert_eq!(base["b"].as_integer(), Some(2));
        // Nested table merged key-by-key: x kept, y overridden, z added.
        assert_eq!(base["t"]["x"].as_integer(), Some(1));
        assert_eq!(base["t"]["y"].as_integer(), Some(9));
        assert_eq!(base["t"]["z"].as_integer(), Some(3));
    }

    #[test]
    fn merge_replaces_scalars_and_arrays_wholesale() {
        // A scalar key is overwritten; an array key is replaced, not concatenated.
        let mut base = val("n = 1\narr = [1, 2, 3]\n");
        merge_toml(&mut base, val("n = 5\narr = [9]\n"));
        assert_eq!(base["n"].as_integer(), Some(5));
        assert_eq!(base["arr"].as_array().unwrap().len(), 1);
        assert_eq!(base["arr"][0].as_integer(), Some(9));
    }

    #[test]
    fn merge_overlay_table_replaces_base_scalar() {
        // Type mismatch (base scalar, overlay table) → overlay wins wholesale.
        let mut base = val("k = 1\n");
        merge_toml(&mut base, val("[k]\ninner = 2\n"));
        assert_eq!(base["k"]["inner"].as_integer(), Some(2));
    }

    // ---- Search-path behaviour (overlays) ------------------------------------

    /// A primary root + one overlay, each optionally carrying a `base.toml` and a
    /// `recipes/<name>.toml`. Returns both tempdirs (kept alive) and the root.
    fn overlaid(
        primary_base: Option<&str>,
        overlay_base: Option<&str>,
    ) -> (tempfile::TempDir, tempfile::TempDir, ConfigRoot) {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        if let Some(b) = primary_base {
            std::fs::write(p.path().join("base.toml"), b).unwrap();
        }
        if let Some(b) = overlay_base {
            std::fs::write(o.path().join("base.toml"), b).unwrap();
        }
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        (p, o, root)
    }

    #[test]
    fn overlay_base_merges_over_shipped() {
        // Primary sets packages + exclude; overlay replaces packages (array
        // wholesale) and leaves exclude untouched (present only in primary).
        let (_p, _o, root) = overlaid(
            Some("packages = [\"a\", \"b\"]\nexclude = [\"x\"]\n"),
            Some("packages = [\"c\"]\n"),
        );
        let base = root.base().unwrap();
        assert_eq!(base.packages, vec!["c"]); // overlay array replaced wholesale
        assert_eq!(base.exclude, vec!["x"]); // untouched key survives the merge
    }

    #[test]
    fn overlay_only_file_resolves_and_lists() {
        // A recipe present only in the overlay resolves and lists alongside the
        // primary's; a reference present in both appears once. Recipes nest one level
        // under their device folder, and `list_recipes` returns `<device>/<leaf>`.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("recipes/d")).unwrap();
        std::fs::create_dir_all(o.path().join("recipes/d")).unwrap();
        std::fs::write(p.path().join("recipes/d/shipped.toml"), "device = \"d\"\n").unwrap();
        std::fs::write(o.path().join("recipes/d/extra.toml"), "device = \"d\"\n").unwrap();
        // `d/shipped` in both roots: overlay adds a suite, must merge, not duplicate.
        std::fs::write(o.path().join("recipes/d/shipped.toml"), "suite = \"sid\"\n").unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        assert_eq!(root.list_recipes().unwrap(), vec!["d/extra", "d/shipped"]);
        let extra = root.recipe("d/extra").unwrap();
        assert_eq!(extra.device, "d");
        // Merged: device from primary, suite from overlay.
        let shipped = root.recipe("d/shipped").unwrap();
        assert_eq!(shipped.device, "d");
        assert_eq!(shipped.suite.as_deref(), Some("sid"));
    }

    #[test]
    fn nested_recipe_ref_addresses_lock_and_manifest_in_the_device_dir() {
        // A `<device>/<leaf>` recipe's lock and manifest sit in the device folder
        // beside the recipe toml, anchored to the root that owns the recipe.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(o.path().join("recipes/turing-rk1")).unwrap();
        std::fs::write(
            o.path().join("recipes/turing-rk1/media-accel-forky.toml"),
            "device = \"turing-rk1\"\n",
        )
        .unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        let rref = "turing-rk1/media-accel-forky";
        assert_eq!(
            root.lock_path(rref).unwrap(),
            o.path().join("recipes/turing-rk1/media-accel-forky.lock")
        );
        assert_eq!(
            root.recipe_sibling(rref, "media-accel-forky.pkgs.lock")
                .unwrap(),
            o.path()
                .join("recipes/turing-rk1/media-accel-forky.pkgs.lock")
        );
        // The manifest filename itself must stay a bare name (no separator)...
        assert!(root.recipe_sibling(rref, "a/b.pkgs.lock").is_err());
        // ...and a two-slash reference is not a valid recipe reference.
        assert!(root.lock_path("a/b/c").is_err());
    }

    #[test]
    fn lock_path_targets_the_owning_root() {
        // A recipe living only in the overlay: its lock write-target lands in the
        // overlay, not the primary root.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(o.path().join("recipes")).unwrap();
        std::fs::write(o.path().join("recipes/ov.toml"), "device = \"d\"\n").unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        let lp = root.lock_path("ov").unwrap();
        assert!(
            lp.starts_with(o.path()),
            "lock should write into the overlay: {lp:?}"
        );
        // A recipe not on the path defaults to the primary root.
        let lp2 = root.lock_path("nowhere").unwrap();
        assert!(lp2.starts_with(p.path()));

        // The manifest sibling anchors to the same owning root as the lock, so an
        // overlay recipe's manifest lands beside its lock rather than in the primary
        // root (Finding 5). A recipe not on the path defaults to the primary root.
        let ms = root.recipe_sibling("ov", "ov.pkgs.lock").unwrap();
        assert!(
            ms.starts_with(o.path()),
            "manifest should write into the overlay: {ms:?}"
        );
        assert!(root
            .recipe_sibling("nowhere", "x.pkgs.lock")
            .unwrap()
            .starts_with(p.path()));
        // A traversal recipe or filename is rejected as a write target.
        assert!(root.recipe_sibling("../x", "m").is_err());
        assert!(root.recipe_sibling("ov", "../m").is_err());
    }

    /// A device file, plus whatever `extra` appends. `image_size` is deliberately not
    /// among the fixed keys: it is the key these tests use to watch inheritance, so a
    /// base-most device states it in `extra` and a variant leaves it to be inherited.
    fn device_toml(name: &str, extra: &str) -> String {
        format!(
            "description = \"{name}\"\nsoc = \"rk3288\"\nboot_method = \"depthcharge\"\n\
             supported_boot_methods = [\"depthcharge\"]\nkernel_dtb = \"rockchip/{name}.dtb\"\n\
             device_config_fragments = []\nsupported_kernels = [\"k\"]\n\
             default_kernel = \"k\"\nsupported_suites = [\"*\"]\n\
             default_suite = \"forky\"\ndefault_layout = \"combined\"\n\
             hostname = \"{name}\"\n{extra}"
        )
    }

    /// A config root holding just `devices/`, for the `extends` walk.
    fn device_root(files: &[(&str, String)]) -> (tempfile::TempDir, ConfigRoot) {
        let p = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("devices")).unwrap();
        for (name, text) in files {
            std::fs::write(p.path().join(format!("devices/{name}.toml")), text).unwrap();
        }
        let root = ConfigRoot::new(p.path().to_path_buf());
        (p, root)
    }

    #[test]
    fn extends_merges_a_parent_under_a_variant_and_reports_the_lineage() {
        let (_tmp, root) = device_root(&[
            (
                "base",
                device_toml("base", "packages = [\"a\", \"b\"]\nimage_size = \"2G\"\n"),
            ),
            (
                "variant",
                // No image_size: it is inherited. An explicit hostname and packages: one
                // scalar and one array the variant does state, to watch both merge rules.
                device_toml("variant", "extends = \"base\"\npackages = [\"c\"]\n"),
            ),
        ]);

        let (d, lineage) = root.device_with_lineage("variant").unwrap();
        // Base-most first: this is the order the devices' assets stack in.
        assert_eq!(lineage, ["base", "variant"]);
        // A key the variant does not state comes from the parent...
        assert_eq!(d.image_size, "2G");
        // ...a key it does state wins...
        assert_eq!(d.hostname, "variant");
        // ...and an array is replaced wholesale, not concatenated, matching how the
        // overlay search path merges. A variant restates what it wants to keep.
        assert_eq!(d.packages, ["c"]);
        assert_eq!(d.extends.as_deref(), Some("base"));

        // The parent still resolves on its own, unaffected by having a variant.
        let (base, base_lineage) = root.device_with_lineage("base").unwrap();
        assert_eq!(base_lineage, ["base"]);
        assert_eq!(base.packages, ["a", "b"]);
        assert!(base.extends.is_none());
    }

    #[test]
    fn extends_walks_a_chain_and_hides_an_ancestors_parent() {
        let (_tmp, root) = device_root(&[
            ("a", device_toml("a", "image_size = \"1G\"\n")),
            ("b", device_toml("b", "extends = \"a\"\n")),
            ("c", device_toml("c", "extends = \"b\"\n")),
        ]);

        let (d, lineage) = root.device_with_lineage("c").unwrap();
        assert_eq!(lineage, ["a", "b", "c"]);
        // Inherited across two hops.
        assert_eq!(d.image_size, "1G");
        // Only the named device's own parent survives the merge: `c` extends `b`, and
        // `b`'s own `extends = "a"` must not read as `c`'s parent.
        assert_eq!(d.extends.as_deref(), Some("b"));
    }

    #[test]
    fn an_extends_cycle_is_named_rather_than_looping() {
        let (_tmp, root) = device_root(&[
            ("x", device_toml("x", "extends = \"y\"\n")),
            ("y", device_toml("y", "extends = \"x\"\n")),
        ]);
        match root.device_with_lineage("x").unwrap_err() {
            ConfigError::DeviceExtendsCycle { device, chain } => {
                assert_eq!(device, "x");
                assert_eq!(
                    chain, "x -> y -> x",
                    "the whole walk, so the bad edge is visible"
                );
            }
            other => panic!("expected DeviceExtendsCycle, got {other:?}"),
        }
        // A device that extends itself is the same failure, not a no-op merge.
        let (_tmp, root) = device_root(&[("s", device_toml("s", "extends = \"s\"\n"))]);
        assert!(matches!(
            root.device_with_lineage("s"),
            Err(ConfigError::DeviceExtendsCycle { .. })
        ));
    }

    #[test]
    fn a_bad_extends_value_or_missing_parent_is_a_named_error() {
        // Not a device name: caught while walking, so the message names the file that
        // holds the value rather than a field on a merged value.
        let (_tmp, root) = device_root(&[("n", device_toml("n", "extends = 3\n"))]);
        match root.device_with_lineage("n").unwrap_err() {
            ConfigError::InvalidDeviceExtends { device, found } => {
                assert_eq!(device, "n");
                assert_eq!(found, "integer");
            }
            other => panic!("expected InvalidDeviceExtends, got {other:?}"),
        }

        // A parent that does not exist is the ordinary not-found, naming the parent.
        let (_tmp, root) = device_root(&[("m", device_toml("m", "extends = \"nope\"\n"))]);
        match root.device_with_lineage("m").unwrap_err() {
            ConfigError::NotFound { kind, name, .. } => {
                assert_eq!((kind, name.as_str()), ("device", "nope"));
            }
            other => panic!("expected NotFound, got {other:?}"),
        }

        // A parent name that could traverse out of `devices/` is rejected before it is
        // joined into a path — the chain walk is a second place a name enters one.
        let (_tmp, root) = device_root(&[("t", device_toml("t", "extends = \"../../etc/x\"\n"))]);
        assert!(matches!(
            root.device_with_lineage("t"),
            Err(ConfigError::InvalidName { kind: "device", .. })
        ));
    }

    #[test]
    fn an_overlay_retunes_a_variant_and_what_it_extends() {
        // Both merge axes compose: the `extends` chain is flattened first, then the
        // search path merges over the result. So an overlay can retune the parent (and
        // have it reach the variant) or the variant alone.
        let p = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("devices")).unwrap();
        std::fs::write(
            p.path().join("devices/base.toml"),
            device_toml("base", "image_size = \"4G\"\n"),
        )
        .unwrap();
        // The variant states no size, so what it reports is whatever it inherited.
        std::fs::write(
            p.path().join("devices/variant.toml"),
            device_toml("variant", "extends = \"base\"\n"),
        )
        .unwrap();

        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(o.path().join("devices")).unwrap();
        // Retune the *parent* only.
        std::fs::write(o.path().join("devices/base.toml"), "image_size = \"9G\"\n").unwrap();

        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        assert_eq!(root.device("base").unwrap().image_size, "9G");
        assert_eq!(
            root.device("variant").unwrap().image_size,
            "9G",
            "an overlay on the parent reaches what extends it"
        );
    }

    /// A minimal parseable lock whose kernel commit is 40 x `commit_char`, for
    /// telling two on-disk locks apart.
    fn lock_toml(commit_char: char) -> String {
        let c: String = std::iter::repeat_n(commit_char, 40).collect();
        let h: String = std::iter::repeat_n('0', 64).collect();
        format!(
            "[kernel]\nid = \"k\"\nsource = \"s\"\nref = \"v\"\ncommit = \"{c}\"\n\
             [uboot]\nsource = \"s\"\nref = \"v\"\ncommit = \"{c}\"\n\
             [rootfs]\nsuite = \"forky\"\nmanifest = \"m.pkgs.lock\"\n\
             [blobs]\natf = \"a.elf@sha256:{h}\"\ntpl = \"t.bin@sha256:{h}\"\n"
        )
    }

    #[test]
    fn lock_reads_from_the_recipe_owning_root() {
        // The lock is addressed by the recipe-owning root for read and write
        // alike: an overlay shipping only a stray lock (without
        // overlaying the recipe) does not shadow the canonical one, so
        // `update`'s write target and `build`'s read source can never diverge.
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("recipes")).unwrap();
        std::fs::create_dir_all(o.path().join("recipes")).unwrap();
        std::fs::write(p.path().join("recipes/r.toml"), "device = \"d\"\n").unwrap();
        std::fs::write(p.path().join("recipes/r.lock"), lock_toml('a')).unwrap();
        std::fs::write(o.path().join("recipes/r.lock"), lock_toml('b')).unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();

        // The primary root owns the recipe, so its lock is the one read...
        assert_eq!(
            root.lock("r").unwrap().kernel.unwrap().commit,
            "a".repeat(40)
        );
        // ...and the write target agrees with the read source.
        assert_eq!(
            root.lock_path("r").unwrap(),
            p.path().join("recipes/r.lock")
        );

        // Overlaying the recipe itself moves ownership — and with it both the
        // lock read and the lock write — to the overlay.
        std::fs::write(o.path().join("recipes/r.toml"), "device = \"d\"\n").unwrap();
        assert_eq!(
            root.lock("r").unwrap().kernel.unwrap().commit,
            "b".repeat(40)
        );
        assert_eq!(
            root.lock_path("r").unwrap(),
            o.path().join("recipes/r.lock")
        );
    }

    #[test]
    fn with_overlays_rejects_an_empty_or_missing_overlay() {
        let primary = tempfile::tempdir().unwrap();
        let good = tempfile::tempdir().unwrap();
        let root = |o: PathBuf| ConfigRoot::with_overlays(primary.path().to_path_buf(), [o]);

        // An existing directory composes the search path.
        assert_eq!(
            root(good.path().to_path_buf())
                .unwrap()
                .search_paths()
                .len(),
            2
        );

        // An empty `--overlay ''` would resolve every asset against the process's
        // current directory — refused, not silently accepted.
        let err = root(PathBuf::new())
            .err()
            .expect("empty overlay is refused");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "the path is empty"),
            "{err}"
        );
        // A typo'd overlay would shadow nothing, so the build would quietly use the
        // shipped config instead of the operator's.
        let err = root(primary.path().join("nope"))
            .err()
            .expect("missing overlay is refused");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "no such directory"),
            "{err}"
        );
        // A file is not a search path.
        let file = primary.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        let err = root(file).err().expect("a file is not an overlay");
        assert!(
            matches!(&err, ConfigError::InvalidOverlay { why, .. } if *why == "not a directory"),
            "{err}"
        );
    }

    #[test]
    fn find_asset_prefers_overlay_and_stacks_all() {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("blob.bin"), "primary").unwrap();
        std::fs::write(o.path().join("blob.bin"), "overlay").unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        // Highest precedence wins.
        assert!(root.find_asset("blob.bin").unwrap().starts_with(o.path()));
        // All copies, low→high (primary first).
        let all = root.find_asset_all("blob.bin");
        assert_eq!(all.len(), 2);
        assert!(all[0].starts_with(p.path()) && all[1].starts_with(o.path()));
        assert!(root.find_asset("absent").is_none());
    }

    #[test]
    fn find_trust_anchor_refuses_overlay_shadow_by_default() {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::write(p.path().join("keyring.gpg"), "shipped").unwrap();
        // No overlay copy: the shipped anchor resolves.
        let root_no_shadow =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        let anchor = root_no_shadow
            .find_trust_anchor("keyring.gpg", false)
            .unwrap()
            .unwrap();
        assert!(
            anchor.starts_with(p.path()),
            "must resolve from the shipped root"
        );

        // An overlay copy is a swap attempt: fail closed.
        std::fs::write(o.path().join("keyring.gpg"), "overlay").unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        assert!(matches!(
            root.find_trust_anchor("keyring.gpg", false),
            Err(ConfigError::OverlayTrustAnchor { .. })
        ));
        // The explicit opt-in lets the overlay's copy win (like find_asset).
        assert!(root
            .find_trust_anchor("keyring.gpg", true)
            .unwrap()
            .unwrap()
            .starts_with(o.path()));
        // Absent everywhere → None (caller falls back to the host trust store).
        assert!(root
            .find_trust_anchor("absent.gpg", false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_kmod_loads_from_its_own_layer_and_merges_across_the_search_path() {
        let p = tempfile::tempdir().unwrap();
        let o = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(p.path().join("kmods")).unwrap();
        std::fs::create_dir_all(o.path().join("kmods")).unwrap();
        std::fs::write(
            p.path().join("kmods/aic8800.toml"),
            "description = \"wifi\"\ngit = \"https://example.invalid/d.git\"\n\
             ref = \"main\"\nsubdir = \"src\"\nmodules = [\"a\"]\n",
        )
        .unwrap();
        let shipped = ConfigRoot::new(p.path().to_path_buf());
        let k = shipped.kmod("aic8800").unwrap();
        assert_eq!(k.git_ref, "main");
        assert_eq!(k.modules, ["a"]);
        // The layer carries no `name`: the stem is the identity, so the file and the
        // thing it declares cannot disagree.
        assert_eq!(k.patch_dir, "debian/patches", "patch_dir defaults");

        // A kmod is a layer like any other, so an overlay retunes one key of it without
        // forking the file — here, pinning the driver at a ref of the operator's own.
        std::fs::write(o.path().join("kmods/aic8800.toml"), "ref = \"my-fork\"\n").unwrap();
        let root =
            ConfigRoot::with_overlays(p.path().to_path_buf(), [o.path().to_path_buf()]).unwrap();
        let merged = root.kmod("aic8800").unwrap();
        assert_eq!(merged.git_ref, "my-fork");
        assert_eq!(merged.modules, ["a"], "unstated keys survive the merge");

        // A missing kmod names the kmod, not the device that asked for it...
        match shipped.kmod("absent").unwrap_err() {
            ConfigError::NotFound { kind, name, .. } => {
                assert_eq!(kind, "kmod");
                assert_eq!(name, "absent");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        // ...and a traversing name never reaches a path join.
        assert!(matches!(
            shipped.kmod("../devices/h96-max-m9"),
            Err(ConfigError::InvalidName { kind: "kmod", .. })
        ));
    }
}
