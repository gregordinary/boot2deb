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
//! [`wrap`] is what watches, on the runs that set
//! [`SandboxRun::probe`](crate::sandbox::SandboxRun::probe). It costs one `sh` and two
//! `tee`s on every run and nothing at all on a successful one, so it captures the
//! *next* occurrence rather than needing an occurrence to be arranged. Four questions,
//! each a grep-able verdict:
//!
//! - **`FOUND-ON-RESTAT`** — the same path opens, in the same mount, moments later.
//!   The file was momentarily unfindable and the mount is intact. The attempt number
//!   it recovered on separates an instant flicker from one that outlasts a sleep.
//! - **`LISTED-NOT-FOUND`** — the parent directory *lists* a name that then does not
//!   resolve. Lookup and `readdir` disagree about one merged directory, which no state
//!   of the underlying layers can explain and the overlay itself therefore must. This
//!   is the signature the two occurrences would have left.
//! - **`PRESENT-NOT-OPENABLE`** — the path stats but will not open, which is a
//!   permission or IO fault and deliberately not folded in with the one above: only a
//!   genuine failure to resolve implicates the overlay.
//! - **`FIRST-ABSENT-COMPONENT`** — the leading path component that does not resolve.
//!   A header under a package's own subdirectory can fail because the *directory* went
//!   missing rather than the file, and the two are different faults.
//! - **`STILL-MISSING`** — it does not open on any attempt. The mount is durably wrong,
//!   which is worse than a transient.
//!
//! Because heavy load is the one variable both occurrences shared and none of the clean
//! audits reproduced, the report also carries the load average and the reclaimable-memory
//! lines of `/proc/meminfo` **read before the retries**, so the machine's state at the
//! moment of failure is recorded rather than inferred.
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
//! Each re-test is a real `open`, not a stat: the fault under investigation is a failed
//! `open`, and a path that stats but will not open is a distinction worth keeping.
//!
//! It deliberately does not wrap the *compiler*. `./configure` writes `CC` into
//! `config.mak`, so an environment `CC` never reaches the compile; reaching it through
//! `--cc=` instead would change the toolchain string the produced `.deb` records. The
//! command is wrapped so the build's own shape stays exactly what it was.

use std::path::Path;

/// The probe's own scratch inside the cage. `/tmp` is the cage's tmpfs, so this is
/// never the overlay under investigation and never reaches the host.
const CAPTURE: &str = "/tmp/boot2deb-probe";

/// How many times a missing path is re-opened before it is called durably absent.
/// The first attempt is immediate, so a path that was never really gone is reported
/// as recovering on attempt 1 rather than being slept over.
const RESTAT_ATTEMPTS: u32 = 10;

/// Wrap `argv` so a non-zero exit re-tests every path the command reported missing,
/// writing the verdict to `report`.
///
/// `report` is a **host** path that must lie under one of the run's binds, since the
/// cage exposes binds at their own absolute path and nothing else survives it.
///
/// The returned argv runs the original command unchanged, on its own two streams, and
/// exits with its status; on success it does no work beyond one `sh`. A stage asks for
/// it through [`SandboxRun::probe`](crate::sandbox::SandboxRun::probe) rather than by
/// calling this directly, so the spec keeps naming the command the build asked for and
/// only what is *launched* carries the wrapper.
pub(crate) fn wrap(argv: &[String], report: &Path) -> Vec<String> {
    let script = SCRIPT
        .replace("@CAPTURE@", CAPTURE)
        .replace("@REPORT@", &report.display().to_string())
        .replace("@ATTEMPTS@", &RESTAT_ATTEMPTS.to_string());
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
///
/// **Each stream stays itself.** The capture is two `tee`s, not one merge: the
/// command's stdout is teed to the log and on to fd 1, its stderr teed to the same log
/// and on to fd 2. A single `2>&1` into one pipe would be shorter and is wrong —
/// [`StepObserver`](crate::sandbox::StepObserver) keeps the last
/// [`STDERR_TAIL`](crate::build::STDERR_TAIL) lines *of stderr* as a failed build's
/// one-line summary, and a merged stderr is an empty one. The compiler's
/// `No such file or directory` is on stderr, so the log the scan below reads has to
/// hold both; the caller's two streams do not.
///
/// `4>&1` inside the outer group carries the caller's stdout past the inner pipeline,
/// which is what lets the inner group send stderr into a pipe (`2>&1`) and *then* put
/// stdout back where it belongs (`1>&4`).
const SCRIPT: &str = r#"
set -u
log=@CAPTURE@.log
rcfile=@CAPTURE@.rc
: > "$log"
{ { "$@"; echo $? >"$rcfile"; } 2>&1 1>&4 | tee -a "$log" >&2; } 4>&1 | tee -a "$log"
rc=$(cat "$rcfile")
[ "$rc" -eq 0 ] && exit 0

# Every path the compiler named as unopenable, deduplicated. gcc and clang both
# render this as `<file>: No such file or directory`.
missing=$(sed -n 's/.*[: ]\([^ :]*\): No such file or directory.*/\1/p' "$log" | sort -u)
[ -n "$missing" ] || exit "$rc"

# The machine's state at the moment of failure, read before any retry so heavy load
# is measured rather than assumed.
loadavg=$(cat /proc/loadavg 2>&1)
meminfo=$(grep -E '^(MemTotal|MemFree|MemAvailable|Cached|Dirty|Writeback|SReclaimable|SUnreclaim):' /proc/meminfo 2>&1)

# The compiler's own search path, asked of the compiler rather than reconstructed:
# the probe must look where the failing command looked.
dirs=$(echo | cc -E -Wp,-v - 2>&1 | sed -n 's/^ \(\/.*\)$/\1/p')

# A real open, because a failed open is the fault under investigation.
openable() { head -c 1 "$1" >/dev/null 2>&1; }

# The leading component of $1 that does not resolve, or nothing when all of them do.
first_absent() {
    _p=""
    _rest=${1#/}
    while [ -n "$_rest" ]; do
        _comp=${_rest%%/*}
        case "$_rest" in
            */*) _rest=${_rest#*/} ;;
            *)   _rest="" ;;
        esac
        _p="$_p/$_comp"
        [ -e "$_p" ] || { echo "$_p"; return; }
    done
}

{
    echo "=== boot2deb lookup probe: $1 exited $rc"
    echo "--- loadavg: $loadavg"
    echo "--- meminfo:"
    echo "$meminfo"
    echo "$missing" | while IFS= read -r name; do
        [ -n "$name" ] || continue

        # Lookup and readdir are asked separately: a name the directory lists but the
        # kernel will not open is the overlay's own inconsistency, and nothing about
        # the underlying layers can account for it.
        for d in $dirs; do
            cand="$d/$name"
            parent=${cand%/*}
            base=${cand##*/}
            if ls -a "$parent" 2>/dev/null | grep -Fxq -- "$base"; then
                if [ -e "$cand" ]; then
                    # Stats but will not open: a permission or IO fault, which is a
                    # real problem and *not* the lookup inconsistency under study.
                    openable "$cand" ||
                        echo "PRESENT-NOT-OPENABLE $name ($cand stats, open fails)"
                else
                    echo "LISTED-NOT-FOUND $name (listed in $parent, does not resolve)"
                fi
            fi
            # Only an *intermediate* component going missing is a finding: a missing
            # leaf is just the file not being in this directory, which is the normal
            # case for every search directory that does not hold it.
            absent=$(first_absent "$cand")
            if [ -n "$absent" ] && [ "$absent" != "$cand" ]; then
                echo "FIRST-ABSENT-COMPONENT $name -> $absent"
            fi
        done

        # Bounded retry: the attempt it recovers on separates a flicker from a fault
        # that outlasts a sleep.
        attempt=0
        hit=
        while [ "$attempt" -lt @ATTEMPTS@ ]; do
            attempt=$((attempt + 1))
            for d in $dirs; do
                if openable "$d/$name"; then hit="$d/$name"; break; fi
            done
            [ -n "$hit" ] && break
            sleep 0.2
        done

        if [ -n "$hit" ]; then
            echo "FOUND-ON-RESTAT $name -> $hit attempt=$attempt"
            ls -l "$hit" 2>&1
            dpkg -S "$hit" 2>&1
        else
            echo "STILL-MISSING $name after $attempt attempt(s) (searched: $(echo $dirs | tr '\n' ' '))"
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

    fn script_for(parts: &[&str]) -> String {
        wrap(&argv(parts), Path::new("/w/r.log"))[2].clone()
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
        assert!(!script.contains("@ATTEMPTS@"));
        assert!(script.contains(r#""$@""#), "the original argv still runs");
    }

    #[test]
    fn the_two_streams_stay_separate_through_the_capture() {
        // The caller keeps the last lines of *stderr* as a failed build's one-line
        // summary, so a `2>&1` merge before the pipe would empty it on exactly the two
        // stages whose configure surfaces are the most brittle. Both streams reach the
        // log the scan reads; neither reaches the other.
        let script = script_for(&["make"]);
        assert!(
            script.contains(
                r#"{ { "$@"; echo $? >"$rcfile"; } 2>&1 1>&4 | tee -a "$log" >&2; } 4>&1 | tee -a "$log""#
            ),
            "the capture is two tees over separate streams"
        );
        assert!(
            !script.contains(r#"{ "$@" 2>&1;"#),
            "the streams are never merged into one pipe"
        );
    }

    #[test]
    fn every_verdict_is_stated_by_name() {
        // Each verdict names a different fault, and a reader greps for the token, so
        // every one of them is a literal in the script.
        let script = script_for(&["make"]);
        for verdict in [
            "FOUND-ON-RESTAT",
            "LISTED-NOT-FOUND",
            "PRESENT-NOT-OPENABLE",
            "FIRST-ABSENT-COMPONENT",
            "STILL-MISSING",
        ] {
            assert!(script.contains(verdict), "{verdict} is not stated");
        }
    }

    #[test]
    fn a_successful_command_exits_before_any_probe_work() {
        let script = script_for(&["make"]);
        let early_exit = script.find(r#"[ "$rc" -eq 0 ] && exit 0"#).expect("guard");
        let first_probe = script.find("No such file or directory").expect("probe");
        assert!(early_exit < first_probe, "the guard precedes the probe");
    }

    #[test]
    fn the_retry_bound_reaches_the_script() {
        // An unbounded retry would hang a failed build instead of reporting it.
        let script = script_for(&["make"]);
        assert!(script.contains(&format!("-lt {RESTAT_ATTEMPTS}")));
    }

    #[test]
    fn load_is_read_before_the_retries_rather_than_after() {
        // The retries take seconds and change the very thing being measured, so the
        // reading has to precede them or it describes the probe, not the failure.
        let script = script_for(&["make"]);
        let load = script
            .find("loadavg=$(cat /proc/loadavg")
            .expect("loadavg read");
        let retry = script.find("attempt=$((attempt + 1))").expect("retry loop");
        assert!(load < retry, "load is sampled before the retry loop runs");
    }

    #[test]
    fn the_restat_is_an_open_rather_than_a_stat() {
        // A stat can succeed where an open fails; the fault under investigation is a
        // failed open, so the re-test has to be one.
        let script = script_for(&["make"]);
        assert!(script.contains(r#"openable() { head -c 1 "$1" >/dev/null 2>&1; }"#));
    }
}
