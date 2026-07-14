//! `patch import` engine side: obtain a patch from a URL or file, and slot a
//! normalized patch into a profile by editing the profile manifest in place —
//! inserting the new label into a scope's ordered array while preserving the
//! file's comments and layout (`toml_edit`).
//!
//! The normalization itself is pure and lives in [`boot2deb_core::mbox`]; the
//! `git am` dry-run verify reuses [`crate::patches`]. This module owns only the two
//! side effects normalization cannot do off-host: the fetch and the manifest edit.

use crate::error::EngineError;
use boot2deb_core::profile::Scope;
use std::path::{Component, Path};
use std::time::Duration;

/// Overall timeout for one patch HTTP fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Body-size cap for one fetched patch. A patch/mbox is text; 64 MiB is
/// far above any real series, so a larger body is refused rather than buffered.
const MAX_PATCH_BYTES: u64 = 64 * 1024 * 1024;

/// Obtain the raw bytes of a patch from `source`: an `http(s)://` URL fetched over
/// pure-Rust TLS (rustls), or any other value read as a local file path.
///
/// A `patchwork.kernel.org` mbox URL and a saved `.patch` file both flow through
/// here; stdin (`-`) is handled by the caller. A transport/HTTP failure or an
/// unreadable file is [`EngineError::PatchImportFetch`].
pub fn fetch(source: &str) -> Result<Vec<u8>, EngineError> {
    if source.starts_with("http://") || source.starts_with("https://") {
        fetch_url(source)
    } else {
        std::fs::read(source).map_err(|e| EngineError::PatchImportFetch {
            source_ref: source.to_string(),
            detail: e.to_string(),
        })
    }
}

/// HTTP(S) GET the full body of `url` under the shared bounded-fetch policy (size
/// cap, no TLS downgrade, bounded redirects).
fn fetch_url(url: &str) -> Result<Vec<u8>, EngineError> {
    crate::netfetch::fetch_bounded(url, MAX_PATCH_BYTES, FETCH_TIMEOUT).map_err(|e| {
        EngineError::PatchImportFetch {
            source_ref: url.to_string(),
            detail: e.0,
        }
    })
}

/// Reject a repo-relative destination label that escapes the patches repo — an
/// absolute path or one with a `..` component. A well-formed label joins
/// safely under the repo root.
pub fn safe_label(label: &str) -> Result<(), EngineError> {
    let path = Path::new(label);
    let escapes = path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)));
    if escapes {
        Err(EngineError::PatchImportUnsafeLabel {
            label: label.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Insert `label` into `scope`'s ordered array in the profile manifest at `profile_path`,
/// at position `index`, preserving the file's comments and layout.
///
/// The manifest is parsed with `toml_edit`, so every comment and blank line outside
/// the edited array is kept byte-for-byte; only the one array grows. A scope key
/// that is absent is created. `index` is clamped to the array length (append). The
/// array is rendered one-label-per-line with a trailing comma, matching the profile
/// convention. A parse failure or a non-array scope value is
/// [`EngineError::PatchImportProfile`].
pub fn insert_into_profile(
    profile_path: &Path,
    scope: Scope,
    index: usize,
    label: &str,
) -> Result<(), EngineError> {
    let text = std::fs::read_to_string(profile_path).map_err(|e| EngineError::io(profile_path, e))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| EngineError::PatchImportProfile {
            path: profile_path.display().to_string(),
            detail: e.to_string(),
        })?;

    let key = scope.as_str();
    // Create the scope array if the manifest omits it (a valid partial profile).
    if doc.get(key).is_none() {
        doc[key] = toml_edit::value(toml_edit::Array::new());
    }
    let arr = doc[key]
        .as_array_mut()
        .ok_or_else(|| EngineError::PatchImportProfile {
            path: profile_path.display().to_string(),
            detail: format!("scope key `{key}` is not an array"),
        })?;

    let at = index.min(arr.len());
    let was_empty = arr.is_empty();
    // Decorate only the inserted element (one label per line, four-space indent),
    // leaving every existing element's formatting — including any inline comment —
    // untouched. A trailing comma matches the profile.toml convention. The array's
    // trailing decor is only rewritten when the array was empty (a formerly-inline
    // `[]` has no comment to lose); on a non-empty array it is preserved, so a
    // trailing inline comment survives.
    let mut value = toml_edit::Value::from(label);
    let decor = value.decor_mut();
    decor.set_prefix("\n    ");
    decor.set_suffix("");
    arr.insert_formatted(at, value);
    arr.set_trailing_comma(true);
    if was_empty {
        arr.set_trailing("\n");
    }

    std::fs::write(profile_path, doc.to_string()).map_err(|e| EngineError::io(profile_path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_label_accepts_repo_relative_and_rejects_escapes() {
        assert!(safe_label("media-accel/kernel/045-x.patch").is_ok());
        assert!(safe_label("rocket/090-y.patch").is_ok());
        assert!(matches!(
            safe_label("../outside.patch"),
            Err(EngineError::PatchImportUnsafeLabel { .. })
        ));
        assert!(matches!(
            safe_label("/etc/passwd"),
            Err(EngineError::PatchImportUnsafeLabel { .. })
        ));
        assert!(matches!(
            safe_label("a/../../b.patch"),
            Err(EngineError::PatchImportUnsafeLabel { .. })
        ));
    }

    #[test]
    fn insert_into_profile_preserves_comments_and_orders() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        let original = "\
applies_to_kernel = \">=7.0, <7.2\"

# Kernel tree comment.
kernel = [
    \"media-accel/kernel/040-a.patch\",
    \"media-accel/kernel/050-b.patch\",
]

# u-boot tree: pristine, no patches.
uboot = []
";
        std::fs::write(&path, original).unwrap();

        // Insert between the two kernel entries.
        insert_into_profile(&path, Scope::Kernel, 1, "media-accel/kernel/045-mid.patch").unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();

        // The comment survived and the new label sits at index 1.
        assert!(edited.contains("# Kernel tree comment."));
        assert!(edited.contains("# u-boot tree: pristine, no patches."));
        let reparsed: boot2deb_core::PatchProfile = toml::from_str(&edited).unwrap();
        assert_eq!(
            reparsed.kernel,
            vec![
                "media-accel/kernel/040-a.patch",
                "media-accel/kernel/045-mid.patch",
                "media-accel/kernel/050-b.patch",
            ]
        );
        // Rendered one-per-line with a trailing comma.
        assert!(edited.contains("    \"media-accel/kernel/045-mid.patch\",\n"));
    }

    #[test]
    fn insert_into_empty_and_appends_past_end() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        std::fs::write(&path, "applies_to_kernel = \">=7.0\"\nuboot = []\n").unwrap();

        // Empty scope, index past the end → the first (and only) element.
        insert_into_profile(&path, Scope::Uboot, 99, "uboot/001-fix.patch").unwrap();
        let reparsed: boot2deb_core::PatchProfile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.uboot, vec!["uboot/001-fix.patch"]);
    }

    #[test]
    fn insert_preserves_an_existing_inline_element_comment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        // An existing element carries a trailing inline comment.
        std::fs::write(
            &path,
            "applies_to_kernel = \">=7.0\"\nkernel = [\n    \"k/040-a.patch\",  # keep me\n]\n",
        )
        .unwrap();

        insert_into_profile(&path, Scope::Kernel, 1, "k/050-b.patch").unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        // The pre-existing inline comment survives the edit.
        assert!(edited.contains("# keep me"), "comment clobbered:\n{edited}");
        let reparsed: boot2deb_core::PatchProfile = toml::from_str(&edited).unwrap();
        assert_eq!(reparsed.kernel, vec!["k/040-a.patch", "k/050-b.patch"]);
    }

    #[test]
    fn insert_creates_a_missing_scope_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        // A partial profile that omits the ffmpeg scope entirely.
        std::fs::write(&path, "applies_to_kernel = \">=7.0\"\nkernel = []\n").unwrap();

        insert_into_profile(&path, Scope::Ffmpeg, 0, "media-accel/ffmpeg/0001-x.patch").unwrap();
        let reparsed: boot2deb_core::PatchProfile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reparsed.ffmpeg, vec!["media-accel/ffmpeg/0001-x.patch"]);
    }
}
