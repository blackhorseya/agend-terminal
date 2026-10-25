//! Build-identity marker shared by the daemon and the `$AGEND_HOME/bin`
//! helpers (`agend-git`, `agend-mcp-bridge`).
//!
//! `cargo install` writes the helpers before the daemon binary, so comparing
//! mtimes flags every fresh install as stale. Instead each helper embeds
//! `AGEND-HELPER-BUILD-ID:<sha>:<dirty>[:<nonce>]\0` (from build.rs's
//! `AGEND_BUILD_SHA` / `AGEND_BUILD_DIRTY` / `AGEND_BUILD_NONCE_SUFFIX`; the
//! nonce exists only for dirty builds, whose content can differ at one sha) and the daemon reads those bytes back out of the helper
//! file — it never executes the helper.
//!
//! Included via `#[path]` by the daemon and by both helper bins; the macros
//! below are the single definition of the marker format.

/// Marker prefix; the parser in `cli.rs` and the embedded marker share it.
#[allow(unused_macros)]
macro_rules! helper_marker_prefix {
    () => {
        "AGEND-HELPER-BUILD-ID:"
    };
}

/// Embed the marker in the calling helper bin and expose `keep_helper_build_id`
/// so `main` can reference it (`black_box`) and stop the linker dropping it.
#[allow(unused_macros)]
macro_rules! embed_helper_build_id {
    () => {
        #[used]
        static HELPER_BUILD_ID: &str = concat!(
            helper_marker_prefix!(),
            env!("AGEND_BUILD_SHA"),
            ":",
            env!("AGEND_BUILD_DIRTY"),
            env!("AGEND_BUILD_NONCE_SUFFIX"),
            "\0"
        );

        fn keep_helper_build_id() {
            std::hint::black_box(HELPER_BUILD_ID);
        }
    };
}
