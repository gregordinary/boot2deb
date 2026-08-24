//! Passive capture of a compile that could not open a file it should have found.
//!
//! A build root is an overlay: a provisioned base as the lower, the stage's own
//! `stage_layer` increment as the upper, and one fresh mount per command
//! ([`BuildRoot::run`](crate::sandbox::BuildRoot::run)). A parallel `make` in such a
//! root has twice hit `fatal error: <header>: No such file or directory` on a header
//! that was present — once for a header the same stage's `./configure` had compiled a
//! probe against minutes earlier. Neither instance reproduced on demand, and both were
//! lost because nothing was watching.
//!
//! [`wrap`] is what watches. It costs one `sh` on every run and nothing at all on a
//! successful one, so it captures the *next* occurrence rather than needing an
//! occurrence to be arranged. Its one question:
//!
//! - **`FOUND-ON-RESTAT`** — the same path opens, in the same mount, moments later.
//!   The file was momentarily unfindable and the mount is intact.
//! - **`STILL-MISSING`** — it does not open either. The mount is durably wrong, which
//!   is a different and worse fault than a transient.
//!
//! The report is written through the stage's read-write bind, so it survives the cage
//! that produced it, and echoed to the command's own output so it reaches the build log
//! unprompted.
//!
//! It re-tests against the **compiler's own default search path**, asked of the
//! compiler rather than reconstructed, not against the failing command's `-I` set — a
//! wrapper around the command cannot see per-invocation flags. Both headers that have
//! gone missing here came from a `-dev` package under `/usr/include`, which is on that
//! path; a `STILL-MISSING` for a header the build reaches only through its own `-I`
//! says nothing, and the recorded search list is printed so a reader can tell the two
//! apart.
//!
//! It deliberately does not wrap the *compiler*. `./configure` writes `CC` into
//! `config.mak`, so an environment `CC` never reaches the compile; reaching it through
//! `--cc=` instead would change the toolchain string the produced `.deb` records. The
//! command is wrapped so the build's own shape stays exactly what it was.

use std::path::Path;

/// The probe's own scratch inside the cage. `/tmp` is the cage's tmpfs, so this is
/// never the overlay under investigation and never reaches the host.
const CAPTURE: &str = "/tmp/boot2deb-probe";

/// Wrap `argv` so a non-zero exit re-tests every path the command reported missing,
/// writing the verdict to `report`.
///
/// `report` is a **host** path that must lie under one of the run's binds, since the
/// cage exposes binds at their own absolute path and nothing else survives it.
///
/// The returned argv runs the original command unchanged and exits with its status; on
/// success it does no work beyond one `sh`. Feed it to
/// [`SandboxRun::argv`](crate::sandbox::SandboxRun::argv) in place of `argv`.
pub(crate) fn wrap(argv: &[String], report: &Path) -> Vec<String> {
    let script = SCRIPT
        .replace("@CAPTURE@", CAPTURE)
        .replace("@REPORT@", &report.display().to_string());
    // `$@` carries the original argv, so nothing is re-quoted into the script and a
    // path holding a shell metacharacter cannot change what runs.
    let mut wrapped = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script,
        "boot2deb-probe".to_string(),
    ];
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// The probe, as POSIX `sh` — no `bash`, so it holds on any Debian base.
///
/// The exit status travels through a file rather than `PIPESTATUS`, which is a
/// `bash`-ism, and the command's output still streams while it is captured.
const SCRIPT: &str = r#"
set -u
log=@CAPTURE@.log
rcfile=@CAPTURE@.rc
{ "$@" 2>&1; echo $? >"$rcfile"; } | tee "$log"
rc=$(cat "$rcfile")
[ "$rc" -eq 0 ] && exit 0

# Every path the compiler named as unopenable, deduplicated. gcc and clang both
# render this as `<file>: No such file or directory`.
missing=$(sed -n 's/.*[: ]\([^ :]*\): No such file or directory.*/\1/p' "$log" | sort -u)
[ -n "$missing" ] || exit "$rc"

# The compiler's own search path, asked of the compiler rather than reconstructed:
# the probe must look where the failing command looked.
dirs=$(echo | cc -E -Wp,-v - 2>&1 | sed -n 's/^ \(\/.*\)$/\1/p')

{
    echo "=== boot2deb lookup probe: $1 exited $rc"
    echo "$missing" | while IFS= read -r name; do
        [ -n "$name" ] || continue
        hit=
        for d in $dirs; do
            [ -e "$d/$name" ] || continue
            hit="$d/$name"
            break
        done
        if [ -n "$hit" ]; then
            echo "FOUND-ON-RESTAT $name -> $hit"
            ls -l "$hit" 2>&1
            dpkg -S "$hit" 2>&1
        else
            echo "STILL-MISSING $name (searched: $(echo $dirs | tr '\n' ' '))"
        fi
    done
    echo "--- mountinfo"
    cat /proc/self/mountinfo
    echo "=== end lookup probe"
} 2>&1 | tee -a "@REPORT@"

exit "$rc"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn the_original_command_is_passed_through_untouched() {
        // The wrapper must not re-quote the command: `$@` carries it, so the argv the
        // build asked for appears verbatim at the tail.
        let original = argv(&["make", "-j32"]);
        let wrapped = wrap(&original, Path::new("/work/ffmpeg/lookup-probe.log"));
        assert_eq!(&wrapped[wrapped.len() - 2..], &original[..]);
        assert_eq!(wrapped[0], "/bin/sh");
        assert_eq!(wrapped[1], "-c");
        // `$0` inside the script, so a shell diagnostic names the probe rather than
        // `sh`.
        assert_eq!(wrapped[3], "boot2deb-probe");
    }

    #[test]
    fn the_report_path_and_capture_dir_are_substituted() {
        let report = PathBuf::from("/work/ffmpeg/lookup-probe.log");
        let wrapped = wrap(&argv(&["make"]), &report);
        let script = &wrapped[2];
        assert!(script.contains("/work/ffmpeg/lookup-probe.log"));
        assert!(script.contains(CAPTURE));
        // `$@` is the script's own, and must survive; the placeholders must not.
        assert!(!script.contains("@REPORT@"));
        assert!(!script.contains("@CAPTURE@"));
        assert!(script.contains(r#""$@""#), "the original argv still runs");
    }

    #[test]
    fn both_verdicts_are_stated_by_name() {
        // The distinction between a transient and a durably wrong mount is the whole
        // point of the probe, so each verdict is a literal grep-able token.
        let script = &wrap(&argv(&["make"]), Path::new("/w/r.log"))[2];
        assert!(script.contains("FOUND-ON-RESTAT"));
        assert!(script.contains("STILL-MISSING"));
    }

    #[test]
    fn a_successful_command_exits_before_any_probe_work() {
        let script = &wrap(&argv(&["make"]), Path::new("/w/r.log"))[2];
        let early_exit = script.find(r#"[ "$rc" -eq 0 ] && exit 0"#).expect("guard");
        let first_probe = script.find("No such file or directory").expect("probe");
        assert!(early_exit < first_probe, "the guard precedes the probe");
    }
}
