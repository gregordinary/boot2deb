// The binary's source paths, `include!`d by both the build script and `src/builder.rs`.
// One list serves the compile-time dirty stamp and the run-time freshness check, so the
// two cannot disagree about which edits change what the binary does.

/// Tracked paths whose content decides what the compiled binary does, relative to the
/// workspace root. A change anywhere else in the repo — a device `.toml`, a `.dts`, a
/// doc page — is build *input*, recorded by the lock and the config stamp, and leaves
/// the binary's identity intact.
const SOURCE_PATHS: [&str; 3] = ["crates", "Cargo.toml", "Cargo.lock"];
