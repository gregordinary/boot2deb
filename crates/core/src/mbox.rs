//! Patch normalization: turn a fetched patch — a `git format-patch` /
//! patchwork mbox, a bare `git show`, or a freeform unified diff — into the
//! canonical `git am`-ready mbox the patches repo stores, so the whole series
//! applies uniformly.
//!
//! Pure: string classification and reshaping only, unit-testable without a host.
//! Fetching the patch (HTTP or file) and running `git am` are engine side effects.
//! A `git format-patch`/patchwork mbox is already canonical, so it is
//! passed through untouched (only a missing `From ` mbox separator is prepended);
//! the freeform shapes are synthesized into the same form, a synthesized commit
//! message wrapping a bare diff.

use crate::error::ConfigError;
use std::collections::BTreeMap;

/// The three shapes a fetched patch can arrive in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    /// A `git format-patch` / patchwork mbox — already `git am`-ready (an mbox
    /// `From ` separator or a `Subject:` email header, then the diff).
    Mbox,
    /// Default `git show` output: a `commit <sha>` header, `Author:`/`Date:`
    /// headers, an indented commit message, then the diff.
    GitShow,
    /// A freeform unified diff (`diff --git …` or `--- a/…` / `+++ b/…`) with no
    /// commit metadata — the message is synthesized on import.
    BareDiff,
}

impl PatchKind {
    /// A short lowercase label for a log line (`"mbox"`, `"git-show"`, `"diff"`).
    pub fn label(self) -> &'static str {
        match self {
            PatchKind::Mbox => "mbox",
            PatchKind::GitShow => "git-show",
            PatchKind::BareDiff => "diff",
        }
    }
}

/// A patch normalized to canonical `git am`-ready mbox text, plus its bare subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// The canonical mbox text, newline-terminated and `git am`-ready.
    pub mbox: String,
    /// The subject with any `[PATCH …]` prefix stripped — the human title used to
    /// derive a filename slug and to label the verify step.
    pub subject: String,
    /// Which input shape produced this, for the caller's log line.
    pub kind: PatchKind,
}

/// Synthesis inputs for the freeform shapes (a subject-less [`PatchKind::BareDiff`]
/// or an author override). Ignored fields on a pass-through [`PatchKind::Mbox`] are
/// noted per field.
#[derive(Debug, Clone, Default)]
pub struct ImportMeta {
    /// `From:` author for a synthesized header. Applied to `git-show` (as a
    /// fallback only) and `bare-diff`; a pass-through mbox keeps its own `From:`.
    pub author: Option<String>,
    /// Subject override — the title for a `bare-diff` that carries none, or an
    /// override for `git-show`. Ignored for a pass-through mbox (it is canonical).
    pub subject: Option<String>,
    /// A DEP-3 `Origin:` provenance trailer to add to the commit message, for
    /// distinguishing local work from a list submission or a backport.
    pub origin: Option<String>,
}

/// The default synthesized `From:` author when [`ImportMeta::author`] is unset.
const DEFAULT_AUTHOR: &str = "boot2deb import <import@boot2deb>";

/// git's fixed magic mbox-separator timestamp (`git format-patch` emits it
/// verbatim); the accompanying sha is informational, so a synthesized patch uses
/// all-zeros.
const MBOX_SEPARATOR_DATE: &str = "Mon Sep 17 00:00:00 2001";

/// Classify a fetched patch by its leading structure.
///
/// Checks the metadata markers before the diff marker, since both a `git show`
/// and an mbox *contain* a `diff --git` further down — only the freeform bare
/// diff *starts* with one.
pub fn classify(text: &str) -> PatchKind {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if is_git_show_header(first) {
        return PatchKind::GitShow;
    }
    // An mbox `From ` separator (note the space — `From:` is a header, not this),
    // or an email whose leading header block carries a `Subject:`.
    if first.starts_with("From ") || has_subject_header(text) {
        return PatchKind::Mbox;
    }
    PatchKind::BareDiff
}

/// Normalize `text` to canonical mbox form, extracting its subject.
pub fn normalize(text: &str, meta: &ImportMeta) -> Result<Normalized, ConfigError> {
    if text.trim().is_empty() {
        return Err(ConfigError::PatchEmpty);
    }
    match classify(text) {
        PatchKind::Mbox => normalize_mbox(text, meta),
        PatchKind::GitShow => normalize_git_show(text, meta),
        PatchKind::BareDiff => normalize_bare_diff(text, meta),
    }
}

/// A URL-and-punctuation-free kebab-case slug for a patch filename, from a subject.
///
/// Lowercases, turns every run of non-alphanumeric characters into a single `-`,
/// trims leading/trailing `-`, and caps the length at a word boundary (≤60 chars)
/// so the numeric prefix stays legible. An all-punctuation subject yields
/// `"patch"`.
pub fn slugify(subject: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in subject.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    // Cap at ~60 chars, backing up to the last `-` so a word is not cut mid-way.
    let capped = if slug.len() <= 60 {
        slug
    } else {
        let cut = slug[..60].rfind('-').unwrap_or(60);
        slug[..cut].trim_matches('-')
    };
    if capped.is_empty() {
        "patch".to_string()
    } else {
        capped.to_string()
    }
}

/// True when a line is a `git show` `commit <sha>` header (sha ≥7 hex, optionally
/// followed by ` (tag: …)` decorations).
fn is_git_show_header(line: &str) -> bool {
    match line.strip_prefix("commit ") {
        Some(rest) => {
            let sha = rest.split_whitespace().next().unwrap_or("");
            sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// True when the leading header block (lines up to the first blank line) carries a
/// `Subject:` header — the reliable mark of an email/mbox even without a `From `
/// separator line.
fn has_subject_header(text: &str) -> bool {
    text.lines()
        .take_while(|l| !l.trim().is_empty())
        .any(|l| l.len() >= 8 && l[..8].eq_ignore_ascii_case("Subject:"))
}

/// Pass a canonical mbox through, prepending a synthetic `From ` separator if the
/// input starts directly with email headers, and splicing an `Origin:` trailer in
/// if requested. `meta.subject`/`meta.author` are ignored — an mbox is canonical.
fn normalize_mbox(text: &str, meta: &ImportMeta) -> Result<Normalized, ConfigError> {
    let text = text.trim_start_matches('\n');
    // Separate an existing `From ` mbox separator from the header/body remainder.
    let (separator, rest) = match text.split_once('\n') {
        Some((first, rest)) if first.starts_with("From ") => (first.to_string(), rest),
        _ => (synth_separator(&"0".repeat(40)), text),
    };
    let headers = parse_email_headers(rest);
    let subject = headers
        .get("subject")
        .map(|s| clean_subject(s))
        .ok_or(ConfigError::PatchMissingSubject)?;
    ensure_has_diff(rest)?;

    let mut mbox = format!("{separator}\n{rest}");
    if let Some(origin) = &meta.origin {
        mbox = splice_origin_trailer(&mbox, origin);
    }
    Ok(Normalized {
        mbox: ensure_trailing_newline(mbox),
        subject,
        kind: PatchKind::Mbox,
    })
}

/// Reshape default `git show` output into canonical mbox: its `commit`/`Author:`/
/// `Date:` become the mbox headers, the indented message is dedented, and the diff
/// is carried through under a synthesized `---` cut.
fn normalize_git_show(text: &str, meta: &ImportMeta) -> Result<Normalized, ConfigError> {
    let mut lines = text.lines();
    let commit_line = lines.next().unwrap_or("");
    let sha = commit_line
        .strip_prefix("commit ")
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("0")
        .to_string();

    // Header block: `Author:`/`Date:`/… up to the first blank line.
    let mut author = None;
    let mut date = None;
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = header_value(line, "Author:") {
            author = Some(v.to_string());
        } else if let Some(v) = header_value(line, "Date:") {
            date = Some(v.to_string());
        }
    }

    // The indented message runs until the diff starts; dedent by up to 4 spaces.
    let mut message_lines = Vec::new();
    let mut diff = String::new();
    let mut rest = String::new();
    for line in lines.by_ref() {
        if is_diff_start(line) {
            diff.push_str(line);
            diff.push('\n');
            rest = lines.collect::<Vec<_>>().join("\n");
            break;
        }
        message_lines.push(dedent(line, 4));
    }
    if !rest.is_empty() {
        diff.push_str(&rest);
        diff.push('\n');
    }
    if diff.trim().is_empty() {
        return Err(ConfigError::PatchNoDiff);
    }

    let (subject_from_msg, body) = split_subject_body(&message_lines);
    let subject = meta
        .subject
        .clone()
        .unwrap_or(subject_from_msg)
        .trim()
        .to_string();
    if subject.is_empty() {
        return Err(ConfigError::PatchMissingSubject);
    }
    let author = author
        .or_else(|| meta.author.clone())
        .unwrap_or_else(|| DEFAULT_AUTHOR.to_string());

    Ok(Normalized {
        mbox: render_mbox(&sha, &author, date.as_deref(), &subject, &body, meta.origin.as_deref(), &diff),
        subject,
        kind: PatchKind::GitShow,
    })
}

/// Wrap a freeform unified diff in a synthesized commit message. The subject
/// comes from `meta.subject`, else is derived from the first changed file.
fn normalize_bare_diff(text: &str, meta: &ImportMeta) -> Result<Normalized, ConfigError> {
    // The diff is the whole payload from its first marker onward.
    let start = text
        .lines()
        .position(is_diff_start)
        .ok_or(ConfigError::PatchNoDiff)?;
    let diff = text.lines().skip(start).collect::<Vec<_>>().join("\n");
    let diff = ensure_trailing_newline(diff);

    let subject = match &meta.subject {
        Some(s) => s.trim().to_string(),
        None => derive_subject(&diff).ok_or(ConfigError::PatchMissingSubject)?,
    };
    let author = meta.author.clone().unwrap_or_else(|| DEFAULT_AUTHOR.to_string());

    Ok(Normalized {
        mbox: render_mbox(&"0".repeat(40), &author, None, &subject, "", meta.origin.as_deref(), &diff),
        subject,
        kind: PatchKind::BareDiff,
    })
}

/// Render canonical mbox from parts. `date` defaults to the fixed epoch when unset
/// so a synthesized patch is deterministic; `origin` adds a trailing DEP-3
/// `Origin:` trailer, separated from the prose body by a blank line.
fn render_mbox(
    sha: &str,
    author: &str,
    date: Option<&str>,
    subject: &str,
    body: &str,
    origin: Option<&str>,
    diff: &str,
) -> String {
    let date = date.unwrap_or("Thu, 1 Jan 1970 00:00:00 +0000");
    let mut out = String::new();
    out.push_str(&synth_separator(sha));
    out.push('\n');
    out.push_str(&format!("From: {author}\n"));
    out.push_str(&format!("Date: {date}\n"));
    out.push_str(&format!("Subject: [PATCH] {subject}\n\n"));

    let body = body.trim_end();
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    if let Some(origin) = origin {
        if !body.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("Origin: {origin}\n"));
    }
    out.push_str("---\n");
    out.push_str(diff);
    ensure_trailing_newline(out)
}

/// The `From <sha> Mon Sep 17 00:00:00 2001` mbox separator line.
fn synth_separator(sha: &str) -> String {
    format!("From {sha} {MBOX_SEPARATOR_DATE}")
}

/// Parse an email header block (up to the first blank line) into a lowercase-keyed
/// map, unfolding RFC 5322 continuation lines (a following line that starts with
/// whitespace continues the previous header's value).
fn parse_email_headers(text: &str) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut last_key: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            // Continuation of the previous header.
            if let Some(key) = &last_key {
                if let Some(v) = headers.get_mut(key) {
                    v.push(' ');
                    v.push_str(line.trim());
                }
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let key = name.trim().to_ascii_lowercase();
            headers.insert(key.clone(), value.trim().to_string());
            last_key = Some(key);
        }
    }
    headers
}

/// Strip a leading `[PATCH …]` (or `[… PATCH …]`) bracket from a subject and trim.
fn clean_subject(subject: &str) -> String {
    let s = subject.trim();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let tag = &rest[..end];
            if tag.to_ascii_uppercase().contains("PATCH") {
                return rest[end + 1..].trim().to_string();
            }
        }
    }
    s.to_string()
}

/// The value of `line` if it is the `name` header (`"Author:"`, `"Date:"`), trimmed.
fn header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.strip_prefix(name).map(str::trim)
}

/// True when a line begins a unified diff (`diff --git`, `diff -`, `--- `, or an
/// `Index:`/`===` git-style file separator).
fn is_diff_start(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("diff -")
        || line.starts_with("--- ")
        || line.starts_with("Index: ")
}

/// Confirm a body contains a diff payload somewhere, so a metadata-only mail is
/// rejected rather than written as an empty patch.
fn ensure_has_diff(text: &str) -> Result<(), ConfigError> {
    if text.lines().any(is_diff_start) {
        Ok(())
    } else {
        Err(ConfigError::PatchNoDiff)
    }
}

/// Remove up to `n` leading spaces from a line (git show indents its message 4).
fn dedent(line: &str, n: usize) -> String {
    let mut chars = line.chars();
    let mut removed = 0;
    let mut out = String::new();
    for ch in chars.by_ref() {
        if removed < n && ch == ' ' {
            removed += 1;
        } else {
            out.push(ch);
            break;
        }
    }
    out.push_str(chars.as_str());
    out
}

/// Split dedented message lines into (subject, body): the first non-blank line is
/// the subject, the remainder (after skipping the blank that follows) is the body.
fn split_subject_body(lines: &[String]) -> (String, String) {
    let first = lines.iter().position(|l| !l.trim().is_empty());
    let Some(start) = first else {
        return (String::new(), String::new());
    };
    let subject = lines[start].trim().to_string();
    let body = lines[start + 1..].join("\n").trim().to_string();
    (subject, body)
}

/// Derive a subject for a bare diff from its first changed file (`diff --git a/P
/// b/P` or `+++ b/P`): `"update <path>"`, or `None` if no path is found.
fn derive_subject(diff: &str) -> Option<String> {
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            if let Some((path, _)) = rest.split_once(" b/") {
                return Some(format!("update {path}"));
            }
        }
        if let Some(rest) = line.strip_prefix("+++ b/") {
            let path = rest.split_whitespace().next().unwrap_or(rest);
            return Some(format!("update {path}"));
        }
    }
    None
}

/// Insert an `Origin:` DEP-3 trailer into an mbox commit message, before the `---`
/// cut (or the first diff line if there is no cut).
fn splice_origin_trailer(mbox: &str, origin: &str) -> String {
    let trailer = format!("Origin: {origin}\n");
    if let Some(pos) = mbox.find("\n---\n") {
        let (head, tail) = mbox.split_at(pos + 1);
        format!("{head}{trailer}{tail}")
    } else if let Some(pos) = mbox.lines().position(is_diff_start) {
        let mut out = String::new();
        for (i, line) in mbox.lines().enumerate() {
            if i == pos {
                out.push_str(&trailer);
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    } else {
        format!("{}{trailer}", ensure_trailing_newline(mbox.to_string()))
    }
}

/// Guarantee a single trailing newline.
fn ensure_trailing_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/foo.c b/foo.c\n\
                        index 111..222 100644\n\
                        --- a/foo.c\n\
                        +++ b/foo.c\n\
                        @@ -1,3 +1,3 @@\n\
                        \x20ctx\n\
                        -old\n\
                        +new\n\
                        \x20ctx2\n";

    #[test]
    fn classifies_the_three_shapes() {
        let mbox = format!(
            "From abc123 Mon Sep 17 00:00:00 2001\n\
             From: A <a@a>\nSubject: [PATCH] x\n\n{DIFF}"
        );
        assert_eq!(classify(&mbox), PatchKind::Mbox);

        // Header-only email (no `From ` separator) is still an mbox.
        let email = format!("From: A <a@a>\nSubject: [PATCH] x\n\n{DIFF}");
        assert_eq!(classify(&email), PatchKind::Mbox);

        let show = format!(
            "commit c9acdc466e9aa96352f658b9276aa8a45b8e817d\n\
             Author: A <a@a>\nDate:   Sat May 9 19:54:04 2026 -0400\n\n    subj\n\n{DIFF}"
        );
        assert_eq!(classify(&show), PatchKind::GitShow);

        assert_eq!(classify(DIFF), PatchKind::BareDiff);
    }

    #[test]
    fn mbox_with_separator_passes_through_and_extracts_subject() {
        let input = format!(
            "From abc123 Mon Sep 17 00:00:00 2001\n\
             From: A <a@a>\nSubject: [PATCH v2 3/5] make foo faster\n\n\
             body line\n---\n{DIFF}"
        );
        let n = normalize(&input, &ImportMeta::default()).unwrap();
        assert_eq!(n.kind, PatchKind::Mbox);
        assert_eq!(n.subject, "make foo faster");
        // Pass-through: the separator was already present, so bytes are unchanged.
        assert_eq!(n.mbox, input);
    }

    #[test]
    fn header_only_email_gets_a_synthetic_separator() {
        let input = format!("From: A <a@a>\nSubject: [PATCH] tidy\n\nmsg\n---\n{DIFF}");
        let n = normalize(&input, &ImportMeta::default()).unwrap();
        assert!(n.mbox.starts_with("From 0000000000000000000000000000000000000000 Mon Sep 17"));
        assert!(n.mbox.contains("Subject: [PATCH] tidy"));
        assert_eq!(n.subject, "tidy");
    }

    #[test]
    fn git_show_is_reshaped_to_mbox() {
        let input = format!(
            "commit c9acdc466e9aa96352f658b9276aa8a45b8e817d (HEAD -> main)\n\
             Author: Jane Dev <jane@example.org>\n\
             Date:   Sat May 9 19:54:04 2026 -0400\n\n\
             \x20\x20\x20\x20rga3: forward-port to 7.1\n\n\
             \x20\x20\x20\x20Three API adjustments.\n\
             \x20\x20\x20\x20Second body line.\n\n{DIFF}"
        );
        let n = normalize(&input, &ImportMeta::default()).unwrap();
        assert_eq!(n.kind, PatchKind::GitShow);
        assert_eq!(n.subject, "rga3: forward-port to 7.1");
        assert!(n.mbox.starts_with(
            "From c9acdc466e9aa96352f658b9276aa8a45b8e817d Mon Sep 17 00:00:00 2001\n"
        ));
        assert!(n.mbox.contains("From: Jane Dev <jane@example.org>\n"));
        assert!(n.mbox.contains("Date: Sat May 9 19:54:04 2026 -0400\n"));
        assert!(n.mbox.contains("Subject: [PATCH] rga3: forward-port to 7.1\n"));
        // Body dedented, and the diff carried under a synthesized `---` cut.
        assert!(n.mbox.contains("\nThree API adjustments.\nSecond body line.\n"));
        assert!(n.mbox.contains("\n---\ndiff --git a/foo.c b/foo.c\n"));
    }

    #[test]
    fn bare_diff_gets_a_synthesized_message() {
        let n = normalize(DIFF, &ImportMeta::default()).unwrap();
        assert_eq!(n.kind, PatchKind::BareDiff);
        // Subject derived from the first changed file.
        assert_eq!(n.subject, "update foo.c");
        assert!(n.mbox.starts_with("From 0000000000000000000000000000000000000000 Mon Sep 17"));
        assert!(n.mbox.contains("From: boot2deb import <import@boot2deb>\n"));
        assert!(n.mbox.contains("Subject: [PATCH] update foo.c\n\n---\n"));
        assert!(n.mbox.contains("diff --git a/foo.c b/foo.c"));
    }

    #[test]
    fn bare_diff_subject_and_author_overrides_apply() {
        let meta = ImportMeta {
            author: Some("Me <me@here>".into()),
            subject: Some("fix the thing".into()),
            origin: None,
        };
        let n = normalize(DIFF, &meta).unwrap();
        assert_eq!(n.subject, "fix the thing");
        assert!(n.mbox.contains("From: Me <me@here>\n"));
        assert!(n.mbox.contains("Subject: [PATCH] fix the thing\n"));
    }

    #[test]
    fn origin_trailer_is_added_to_synthesized_and_mbox() {
        let meta = ImportMeta {
            origin: Some("https://patchwork.kernel.org/patch/42".into()),
            ..Default::default()
        };
        // Synthesized (bare diff): trailer sits in the message, before `---`.
        let n = normalize(DIFF, &meta).unwrap();
        let msg = n.mbox.split("\n---\n").next().unwrap();
        assert!(msg.contains("Origin: https://patchwork.kernel.org/patch/42"));

        // Pass-through mbox: trailer spliced before the existing `---`.
        let input = format!("From: A <a@a>\nSubject: [PATCH] x\n\nbody\n---\n{DIFF}");
        let n = normalize(&input, &meta).unwrap();
        let (head, tail) = n.mbox.split_once("\n---\n").unwrap();
        assert!(head.contains("Origin: https://patchwork.kernel.org/patch/42"));
        assert!(tail.contains("diff --git"));
    }

    #[test]
    fn empty_and_diffless_inputs_are_rejected() {
        assert!(matches!(normalize("   \n", &ImportMeta::default()), Err(ConfigError::PatchEmpty)));
        let no_diff = "From: A <a@a>\nSubject: [PATCH] x\n\njust prose, no diff\n";
        assert!(matches!(normalize(no_diff, &ImportMeta::default()), Err(ConfigError::PatchNoDiff)));
    }

    #[test]
    fn bare_diff_without_derivable_subject_errors() {
        // A `---`/`+++`-only diff with a non-`b/` target has no file to name.
        let odd = "--- one\n+++ two\n@@ -1 +1 @@\n-a\n+b\n";
        let err = normalize(odd, &ImportMeta::default()).unwrap_err();
        assert!(matches!(err, ConfigError::PatchMissingSubject));
    }

    #[test]
    fn slugify_is_kebab_and_capped() {
        assert_eq!(
            slugify("lavfi/rkrga: accept Kwiboo v4l2-request 10-bit (NV15/NV20)"),
            "lavfi-rkrga-accept-kwiboo-v4l2-request-10-bit-nv15-nv20"
        );
        assert_eq!(slugify("  Trim -- Me  "), "trim-me");
        assert_eq!(slugify("!!!"), "patch");
        // Long subject caps at a word boundary, ≤60 chars.
        let long = slugify("one two three four five six seven eight nine ten eleven twelve thirteen");
        assert!(long.len() <= 60, "len {}", long.len());
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn clean_subject_strips_patch_brackets_only() {
        assert_eq!(clean_subject("[PATCH] hello"), "hello");
        assert_eq!(clean_subject("[PATCH v3 2/7] hello"), "hello");
        // A non-PATCH bracket is left intact.
        assert_eq!(clean_subject("[media] fix"), "[media] fix");
    }

    #[test]
    fn round_trips_a_realistic_patchwork_mbox_unchanged() {
        // Exactly what patchwork serves: a leading `From ` line, full headers, a
        // multi-line folded subject, a body, a `---` diffstat cut, then the diff.
        let input = "From 7829ae2a Mon Sep 17 00:00:00 2001\n\
             From: RK Dev <rk@dev>\n\
             Date: Sat, 16 May 2026 13:39:59 -0400\n\
             Subject: [PATCH] lavfi/rkrga: accept v4l2-request 10-bit\n \
             (NV15/NV20)\n\n\
             Longer explanation of the change.\n\
             ---\n \
             libavfilter/vf_scale_rkrga.c | 4 ++--\n \
             1 file changed, 2 insertions(+), 2 deletions(-)\n\n\
             diff --git a/libavfilter/vf_scale_rkrga.c b/libavfilter/vf_scale_rkrga.c\n\
             --- a/libavfilter/vf_scale_rkrga.c\n\
             +++ b/libavfilter/vf_scale_rkrga.c\n\
             @@ -1 +1 @@\n-x\n+y\n";
        let n = normalize(input, &ImportMeta::default()).unwrap();
        // Folded subject unfolded for the label.
        assert_eq!(n.subject, "lavfi/rkrga: accept v4l2-request 10-bit (NV15/NV20)");
        // Canonical, byte-for-byte pass-through.
        assert_eq!(n.mbox, input);
    }
}
